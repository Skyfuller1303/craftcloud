//! Typed server configuration, read from `CRAFTCLOUD_*` environment variables
//! at the server edge. Library crates never read the environment.
//!
//! Parsing is a pure function over a key/value map so it can be tested without
//! touching the process environment. All problems are collected and reported
//! together. Error messages never contain the value of keys that may hold
//! credentials (`PUBLIC_URL` may carry userinfo, the reserved keys carry
//! connection strings or secret paths).

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use ipnet::IpNet;
use url::{Host, Url};

const PREFIX: &str = "CRAFTCLOUD_";

const DEFAULT_BIND: &str = "127.0.0.1:8080";
const DEFAULT_MAX_BODY_BYTES: u64 = 64 * 1024;
const MAX_BODY_BYTES_RANGE: (u64, u64) = (1024, 16 * 1024 * 1024);
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 10;
const REQUEST_TIMEOUT_SECS_RANGE: (u64, u64) = (1, 300);
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 10;
const SHUTDOWN_GRACE_SECS_RANGE: (u64, u64) = (1, 120);
const MAX_TRUSTED_PROXY_ENTRIES: usize = 64;
const DEFAULT_DB_MAX_CONNECTIONS: u64 = 10;
const DB_MAX_CONNECTIONS_RANGE: (u64, u64) = (1, 100);
const DEFAULT_DB_CONNECT_TIMEOUT_SECS: u64 = 5;
const DB_CONNECT_TIMEOUT_SECS_RANGE: (u64, u64) = (1, 60);

/// Keys reserved for later milestones. They are rejected explicitly instead
/// of being silently ignored.
const RESERVED_KEYS: [&str; 5] = [
    "DATABASE_URL",
    "STORAGE_BACKEND",
    "STORAGE_ROOT",
    "BOOTSTRAP_SECRET_FILE",
    "SESSION_SECRET_FILE",
];

const SUPPORTED_KEYS: [&str; 10] = [
    "BIND",
    "PUBLIC_URL",
    "TRUSTED_PROXIES",
    "MAX_BODY_BYTES",
    "REQUEST_TIMEOUT_SECS",
    "SHUTDOWN_GRACE_SECS",
    "LOG_FORMAT",
    "DATABASE_URL_FILE",
    "DATABASE_MAX_CONNECTIONS",
    "DATABASE_CONNECT_TIMEOUT_SECS",
];

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human readable lines.
    Text,
    /// One JSON object per line.
    Json,
}

/// Database settings. The URL itself is never configured directly because it
/// carries a password; it is read from a secret file at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseSettings {
    /// Absolute path of the secret file holding the PostgreSQL URL. `None`
    /// means no database is configured (prototype mode).
    pub url_file: Option<PathBuf>,
    /// Maximum pool size.
    pub max_connections: u32,
    /// Time allowed to establish a connection.
    pub connect_timeout: Duration,
}

/// One configuration problem, tied to a key name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// Full environment variable name, e.g. `CRAFTCLOUD_BIND`.
    pub key: String,
    /// Description of the problem; free of secret values.
    pub message: String,
}

impl ConfigError {
    fn new(key: &str, message: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

/// Validated server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Socket address to listen on.
    pub bind: SocketAddr,
    /// Externally visible base URL without trailing slash, if configured.
    /// The only source for absolute URLs; never derived from request headers.
    pub public_url: Option<String>,
    /// Proxies whose `X-Forwarded-For` is trusted. Empty means trust nobody.
    pub trusted_proxies: Vec<IpNet>,
    /// Maximum request body size in bytes.
    pub max_body_bytes: usize,
    /// Handler timeout.
    pub request_timeout: Duration,
    /// Time allowed for in-flight requests after a shutdown signal.
    pub shutdown_grace: Duration,
    /// Log output format.
    pub log_format: LogFormat,
    /// Database settings.
    pub database: DatabaseSettings,
}

impl Config {
    /// Reads the process environment. Only `CRAFTCLOUD_*` variables are
    /// considered; others are ignored.
    pub fn from_process_env() -> Result<Self, Vec<ConfigError>> {
        let mut map = BTreeMap::new();
        let mut errors = Vec::new();
        for (key, value) in std::env::vars_os() {
            let key_lossy = key.to_string_lossy().into_owned();
            if !has_prefix(&key_lossy) {
                continue;
            }
            match (key.to_str(), value.to_str()) {
                (Some(k), Some(v)) => {
                    map.insert(k.to_owned(), v.to_owned());
                }
                _ => errors.push(ConfigError::new(
                    &key_lossy,
                    "name or value is not valid unicode",
                )),
            }
        }
        match Self::from_env_map(&map) {
            Ok(config) if errors.is_empty() => Ok(config),
            Ok(_) => Err(errors),
            Err(mut more) => {
                errors.append(&mut more);
                Err(errors)
            }
        }
    }

    /// Validates a key/value map. Collects every error.
    pub fn from_env_map(env: &BTreeMap<String, String>) -> Result<Self, Vec<ConfigError>> {
        let mut errors = Vec::new();

        for key in env.keys().filter(|k| has_prefix(k)) {
            let suffix = key.get(PREFIX.len()..).unwrap_or("");
            if RESERVED_KEYS.contains(&suffix) {
                let message = if suffix == "DATABASE_URL" {
                    "not supported in this build yet: database URLs contain passwords; put the URL in a secret file and set CRAFTCLOUD_DATABASE_URL_FILE to its absolute path"
                } else {
                    "not supported in this build yet (reserved for a later milestone)"
                };
                errors.push(ConfigError::new(key, message));
            } else if !SUPPORTED_KEYS.contains(&suffix) || key.get(..PREFIX.len()) != Some(PREFIX) {
                errors.push(ConfigError::new(
                    key,
                    format!(
                        "unknown configuration key; supported keys: {}",
                        supported_list()
                    ),
                ));
            }
        }

        let bind = field(&mut errors, env, "BIND", |key, raw| {
            raw.parse::<SocketAddr>().map_err(|err| {
                ConfigError::new(
                    key,
                    format!(
                        "invalid value {raw:?}: {err}; expected <ip>:<port>, e.g. {DEFAULT_BIND}"
                    ),
                )
            })
        })
        .unwrap_or_else(default_bind);

        let public_url = field(&mut errors, env, "PUBLIC_URL", parse_public_url);

        let trusted_proxies =
            field(&mut errors, env, "TRUSTED_PROXIES", parse_trusted_proxies).unwrap_or_default();

        let max_body_bytes = field(&mut errors, env, "MAX_BODY_BYTES", |key, raw| {
            parse_ranged(key, raw, MAX_BODY_BYTES_RANGE)
        })
        .unwrap_or(DEFAULT_MAX_BODY_BYTES);

        let request_timeout_secs = field(&mut errors, env, "REQUEST_TIMEOUT_SECS", |key, raw| {
            parse_ranged(key, raw, REQUEST_TIMEOUT_SECS_RANGE)
        })
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);

        let shutdown_grace_secs = field(&mut errors, env, "SHUTDOWN_GRACE_SECS", |key, raw| {
            parse_ranged(key, raw, SHUTDOWN_GRACE_SECS_RANGE)
        })
        .unwrap_or(DEFAULT_SHUTDOWN_GRACE_SECS);

        let log_format = field(&mut errors, env, "LOG_FORMAT", |key, raw| match raw {
            "text" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            other => Err(ConfigError::new(
                key,
                format!("invalid value {other:?}; expected \"text\" or \"json\""),
            )),
        })
        .unwrap_or(LogFormat::Text);

        let db_url_file = field(&mut errors, env, "DATABASE_URL_FILE", |key, raw| {
            let path = PathBuf::from(raw.trim());
            if raw.trim().is_empty() || !path.is_absolute() {
                Err(ConfigError::new(
                    key,
                    "must be an absolute path to a secret file",
                ))
            } else {
                Ok(path)
            }
        });
        let db_max_connections = field(&mut errors, env, "DATABASE_MAX_CONNECTIONS", |key, raw| {
            parse_ranged(key, raw, DB_MAX_CONNECTIONS_RANGE)
        })
        .unwrap_or(DEFAULT_DB_MAX_CONNECTIONS);
        let db_connect_timeout_secs = field(
            &mut errors,
            env,
            "DATABASE_CONNECT_TIMEOUT_SECS",
            |key, raw| parse_ranged(key, raw, DB_CONNECT_TIMEOUT_SECS_RANGE),
        )
        .unwrap_or(DEFAULT_DB_CONNECT_TIMEOUT_SECS);

        // Range-checked above, so the conversion cannot fail; the fallback
        // keeps the code free of panics regardless.
        let max_body_bytes = usize::try_from(max_body_bytes).unwrap_or(usize::MAX);

        if errors.is_empty() {
            Ok(Self {
                bind,
                public_url,
                trusted_proxies,
                max_body_bytes,
                request_timeout: Duration::from_secs(request_timeout_secs),
                shutdown_grace: Duration::from_secs(shutdown_grace_secs),
                log_format,
                database: DatabaseSettings {
                    url_file: db_url_file,
                    max_connections: u32::try_from(db_max_connections).unwrap_or(u32::MAX),
                    connect_timeout: Duration::from_secs(db_connect_timeout_secs),
                },
            })
        } else {
            Err(errors)
        }
    }
}

fn has_prefix(key: &str) -> bool {
    key.get(..PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(PREFIX))
}

fn supported_list() -> String {
    SUPPORTED_KEYS
        .iter()
        .map(|k| format!("{PREFIX}{k}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

/// Looks up `CRAFTCLOUD_<name>` and parses it; records an error on failure.
fn field<T>(
    errors: &mut Vec<ConfigError>,
    env: &BTreeMap<String, String>,
    name: &str,
    parse: impl FnOnce(&str, &str) -> Result<T, ConfigError>,
) -> Option<T> {
    let key = format!("{PREFIX}{name}");
    let raw = env.get(&key)?;
    match parse(&key, raw) {
        Ok(value) => Some(value),
        Err(err) => {
            errors.push(err);
            None
        }
    }
}

fn parse_ranged(key: &str, raw: &str, (min, max): (u64, u64)) -> Result<u64, ConfigError> {
    let value: u64 = raw.trim().parse().map_err(|_| {
        ConfigError::new(
            key,
            format!("invalid value {raw:?}; expected an integer between {min} and {max}"),
        )
    })?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(ConfigError::new(
            key,
            format!("value {value} is out of range; expected an integer between {min} and {max}"),
        ))
    }
}

/// The value is deliberately never echoed: a URL may contain credentials.
fn parse_public_url(key: &str, raw: &str) -> Result<String, ConfigError> {
    let bad = |why: &str| Err(ConfigError::new(key, format!("{why} (value not shown)")));
    let Ok(url) = Url::parse(raw.trim()) else {
        return bad("must be an absolute URL such as https://cloud.example.org");
    };
    if !url.username().is_empty() || url.password().is_some() {
        return bad("must not contain user information");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return bad("must not contain a query or fragment");
    }
    let Some(host) = url.host() else {
        return bad("must contain a host");
    };
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(&host) => {}
        "http" => {
            return bad(
                "scheme http is only allowed for loopback hosts (localhost, 127.0.0.0/8, ::1); use https",
            );
        }
        _ => return bad("scheme must be https"),
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn is_loopback_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

fn parse_trusted_proxies(key: &str, raw: &str) -> Result<Vec<IpNet>, ConfigError> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() > MAX_TRUSTED_PROXY_ENTRIES {
        return Err(ConfigError::new(
            key,
            format!("too many entries; at most {MAX_TRUSTED_PROXY_ENTRIES} are allowed"),
        ));
    }
    let mut nets = Vec::with_capacity(parts.len());
    for part in parts {
        let net = parse_net(part).ok_or_else(|| {
            ConfigError::new(
                key,
                format!(
                    "invalid entry {part:?}; expected an IP address or CIDR such as 10.0.0.0/8"
                ),
            )
        })?;
        if net.prefix_len() == 0 {
            return Err(ConfigError::new(
                key,
                format!(
                    "entry {part:?} would trust every client address; any client could then spoof \
                     X-Forwarded-For. List only your real proxies"
                ),
            ));
        }
        nets.push(net);
    }
    Ok(nets)
}

fn parse_net(part: &str) -> Option<IpNet> {
    part.parse::<IpNet>()
        .ok()
        .or_else(|| part.parse::<IpAddr>().ok().map(IpNet::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn errors_of(pairs: &[(&str, &str)]) -> Vec<ConfigError> {
        Config::from_env_map(&env(pairs)).unwrap_err()
    }

    fn keys(errors: &[ConfigError]) -> Vec<&str> {
        errors.iter().map(|e| e.key.as_str()).collect()
    }

    #[test]
    fn defaults() {
        let config = Config::from_env_map(&BTreeMap::new()).unwrap();
        assert_eq!(config.bind, default_bind());
        assert_eq!(config.public_url, None);
        assert!(config.trusted_proxies.is_empty());
        assert_eq!(config.max_body_bytes, 64 * 1024);
        assert_eq!(config.request_timeout, Duration::from_secs(10));
        assert_eq!(config.shutdown_grace, Duration::from_secs(10));
        assert_eq!(config.log_format, LogFormat::Text);
        assert_eq!(config.database.url_file, None);
        assert_eq!(config.database.max_connections, 10);
        assert_eq!(config.database.connect_timeout, Duration::from_secs(5));
    }

    #[test]
    fn database_keys() {
        let path = if cfg!(windows) {
            r"C:\run\secrets\db_url"
        } else {
            "/run/secrets/db_url"
        };
        let config = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_DATABASE_URL_FILE", path),
            ("CRAFTCLOUD_DATABASE_MAX_CONNECTIONS", "100"),
            ("CRAFTCLOUD_DATABASE_CONNECT_TIMEOUT_SECS", "60"),
        ]))
        .unwrap();
        assert_eq!(config.database.url_file, Some(PathBuf::from(path)));
        assert_eq!(config.database.max_connections, 100);
        assert_eq!(config.database.connect_timeout, Duration::from_secs(60));
        for (key, bad) in [
            ("CRAFTCLOUD_DATABASE_URL_FILE", "relative/path"),
            ("CRAFTCLOUD_DATABASE_URL_FILE", ""),
            ("CRAFTCLOUD_DATABASE_MAX_CONNECTIONS", "0"),
            ("CRAFTCLOUD_DATABASE_MAX_CONNECTIONS", "101"),
            ("CRAFTCLOUD_DATABASE_CONNECT_TIMEOUT_SECS", "0"),
            ("CRAFTCLOUD_DATABASE_CONNECT_TIMEOUT_SECS", "61"),
            ("CRAFTCLOUD_DATABASE_CONNECT_TIMEOUT_SECS", "x"),
        ] {
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
    }

    #[test]
    fn database_url_error_points_to_file_key() {
        let errors = errors_of(&[("CRAFTCLOUD_DATABASE_URL", "postgres://u:pw0rd@h/db")]);
        assert!(errors[0].message.contains("CRAFTCLOUD_DATABASE_URL_FILE"));
        assert!(!errors[0].to_string().contains("pw0rd"));
    }

    #[test]
    fn non_craftcloud_variables_are_ignored() {
        assert!(Config::from_env_map(&env(&[("PATH", "x"), ("CRAFT_X", "y")])).is_ok());
    }

    #[test]
    fn all_values_set() {
        let config = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_BIND", "0.0.0.0:9000"),
            ("CRAFTCLOUD_PUBLIC_URL", "https://cloud.example.org/"),
            ("CRAFTCLOUD_TRUSTED_PROXIES", "10.0.0.0/8, 192.168.1.5 ,::1"),
            ("CRAFTCLOUD_MAX_BODY_BYTES", "2048"),
            ("CRAFTCLOUD_REQUEST_TIMEOUT_SECS", "30"),
            ("CRAFTCLOUD_SHUTDOWN_GRACE_SECS", "5"),
            ("CRAFTCLOUD_LOG_FORMAT", "json"),
        ]))
        .unwrap();
        assert_eq!(config.bind.port(), 9000);
        assert_eq!(
            config.public_url.as_deref(),
            Some("https://cloud.example.org")
        );
        assert_eq!(config.trusted_proxies.len(), 3);
        assert_eq!(config.max_body_bytes, 2048);
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.shutdown_grace, Duration::from_secs(5));
        assert_eq!(config.log_format, LogFormat::Json);
    }

    #[test]
    fn collects_all_errors() {
        let errors = errors_of(&[
            ("CRAFTCLOUD_BIND", "garbage"),
            ("CRAFTCLOUD_TYPO", "1"),
            ("CRAFTCLOUD_PUBLIC_URL", "http://example.com"),
        ]);
        assert_eq!(
            keys(&errors),
            [
                "CRAFTCLOUD_TYPO",
                "CRAFTCLOUD_BIND",
                "CRAFTCLOUD_PUBLIC_URL"
            ]
        );
    }

    #[test]
    fn unknown_and_miscased_keys_are_errors() {
        let errors = errors_of(&[("CRAFTCLOUD_BIDN", "x"), ("craftcloud_bind", "127.0.0.1:1")]);
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|e| e.message.contains("unknown")));
    }

    #[test]
    fn reserved_keys_are_rejected_without_values() {
        for key in RESERVED_KEYS {
            let full = format!("{PREFIX}{key}");
            let errors = errors_of(&[(full.as_str(), "postgres://user:s3cr3t@db/x")]);
            assert_eq!(errors.len(), 1);
            assert!(
                errors[0]
                    .message
                    .contains("not supported in this build yet")
            );
            assert!(!errors[0].to_string().contains("s3cr3t"));
        }
    }

    fn public_url(value: &str) -> Result<Option<String>, Vec<ConfigError>> {
        Config::from_env_map(&env(&[("CRAFTCLOUD_PUBLIC_URL", value)])).map(|c| c.public_url)
    }

    #[test]
    fn public_url_accepts_https_and_normalizes() {
        assert_eq!(
            public_url("https://cloud.example.org/").unwrap().as_deref(),
            Some("https://cloud.example.org")
        );
        assert_eq!(
            public_url("https://cloud.example.org:8443/cc/")
                .unwrap()
                .as_deref(),
            Some("https://cloud.example.org:8443/cc")
        );
    }

    #[test]
    fn public_url_allows_http_only_for_loopback() {
        for ok in [
            "http://localhost:8080",
            "http://LOCALHOST",
            "http://127.0.0.1:8080",
            "http://127.9.9.9",
            "http://[::1]:8080",
        ] {
            assert!(public_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.com",
            "http://10.0.0.1",
            "http://localhost.evil.com",
        ] {
            assert!(public_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn public_url_rejects_bad_shapes_without_echoing_value() {
        for bad in [
            "",
            "cloud.example.org",
            "ftp://example.com",
            "https://user:hunter2@example.com",
            "https://user@example.com",
            "https://example.com/?token=hunter2",
            "https://example.com/#hunter2",
            "file:///etc/passwd",
            "https://",
        ] {
            let errors = public_url(bad).unwrap_err();
            assert_eq!(errors.len(), 1, "{bad}");
            let text = errors[0].to_string();
            assert!(!text.contains("hunter2"), "{text}");
            assert!(!text.contains("example.com"), "{text}");
        }
    }

    #[test]
    fn trusted_proxies_parse_ips_and_cidrs() {
        let config = Config::from_env_map(&env(&[(
            "CRAFTCLOUD_TRUSTED_PROXIES",
            "127.0.0.1, 10.0.0.0/8,fd00::/8",
        )]))
        .unwrap();
        assert_eq!(config.trusted_proxies.len(), 3);
        let empty = Config::from_env_map(&env(&[("CRAFTCLOUD_TRUSTED_PROXIES", " ")])).unwrap();
        assert!(empty.trusted_proxies.is_empty());
    }

    #[test]
    fn trusted_proxies_reject_trust_all_and_garbage() {
        for bad in [
            "0.0.0.0/0",
            "::/0",
            "10.0.0.0/8,0.0.0.0/0",
            "nope",
            "10.0.0.0/33",
            "1.1.1.1,,2.2.2.2",
        ] {
            let errors = errors_of(&[("CRAFTCLOUD_TRUSTED_PROXIES", bad)]);
            assert_eq!(keys(&errors), ["CRAFTCLOUD_TRUSTED_PROXIES"], "{bad}");
        }
        let errors = errors_of(&[("CRAFTCLOUD_TRUSTED_PROXIES", "0.0.0.0/0")]);
        assert!(errors[0].message.contains("spoof"));
        let many = vec!["10.0.0.1"; MAX_TRUSTED_PROXY_ENTRIES + 1].join(",");
        assert!(!errors_of(&[("CRAFTCLOUD_TRUSTED_PROXIES", many.as_str())]).is_empty());
    }

    #[test]
    fn numeric_ranges_are_enforced() {
        let cases = [
            (
                "CRAFTCLOUD_MAX_BODY_BYTES",
                [
                    "1023",
                    "16777217",
                    "-1",
                    "abc",
                    "",
                    "99999999999999999999999",
                ],
            ),
            (
                "CRAFTCLOUD_REQUEST_TIMEOUT_SECS",
                ["0", "301", "-1", "1.5", "", "99999999999999999999999"],
            ),
            (
                "CRAFTCLOUD_SHUTDOWN_GRACE_SECS",
                ["0", "121", "-1", "x", "", "99999999999999999999999"],
            ),
        ];
        for (key, values) in cases {
            for value in values {
                let errors = errors_of(&[(key, value)]);
                assert_eq!(keys(&errors), [key], "{key}={value}");
            }
        }
    }

    #[test]
    fn numeric_bounds_are_inclusive() {
        let config = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_MAX_BODY_BYTES", "16777216"),
            ("CRAFTCLOUD_REQUEST_TIMEOUT_SECS", "1"),
            ("CRAFTCLOUD_SHUTDOWN_GRACE_SECS", "120"),
        ]))
        .unwrap();
        assert_eq!(config.max_body_bytes, 16 * 1024 * 1024);
        assert_eq!(config.request_timeout, Duration::from_secs(1));
        assert_eq!(config.shutdown_grace, Duration::from_secs(120));
    }

    #[test]
    fn log_format_values() {
        assert!(Config::from_env_map(&env(&[("CRAFTCLOUD_LOG_FORMAT", "text")])).is_ok());
        for bad in ["JSON", "xml", ""] {
            assert!(!errors_of(&[("CRAFTCLOUD_LOG_FORMAT", bad)]).is_empty());
        }
    }

    #[test]
    fn bind_errors_name_the_key() {
        for bad in ["garbage", "", "127.0.0.1", "127.0.0.1:99999", ":8080"] {
            let errors = errors_of(&[("CRAFTCLOUD_BIND", bad)]);
            assert_eq!(keys(&errors), ["CRAFTCLOUD_BIND"]);
        }
    }
}
