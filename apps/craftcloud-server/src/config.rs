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
const DEFAULT_SESSION_IDLE_MINUTES: u64 = 8 * 60;
const SESSION_IDLE_MINUTES_RANGE: (u64, u64) = (5, 72 * 60);
const DEFAULT_SESSION_ABSOLUTE_HOURS: u64 = 7 * 24;
const SESSION_ABSOLUTE_HOURS_RANGE: (u64, u64) = (1, 30 * 24);
const DEFAULT_TRASH_RETENTION_DAYS: u64 = 30;
const DEFAULT_UPLOAD_TTL_HOURS: u64 = 24;
const UPLOAD_TTL_HOURS_RANGE: (u64, u64) = (1, 168);
const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_FILE_SIZE_RANGE: (u64, u64) = (1024 * 1024, 1024 * 1024 * 1024 * 1024);
const TRASH_RETENTION_DAYS_RANGE: (u64, u64) = (1, 3650);
const DEFAULT_DB_MAX_CONNECTIONS: u64 = 10;
const DB_MAX_CONNECTIONS_RANGE: (u64, u64) = (1, 100);
const DEFAULT_DB_CONNECT_TIMEOUT_SECS: u64 = 5;
const DB_CONNECT_TIMEOUT_SECS_RANGE: (u64, u64) = (1, 60);
const DEFAULT_MAINTENANCE_INTERVAL_SECS: u64 = 300;
const MAINTENANCE_INTERVAL_SECS_RANGE: (u64, u64) = (10, 86_400);
const DEFAULT_VERSION_RETENTION_DAYS: u64 = 30;
const VERSION_RETENTION_DAYS_RANGE: (u64, u64) = (1, 3650);
const DEFAULT_QUARANTINE_DAYS: u64 = 30;
const QUARANTINE_DAYS_RANGE: (u64, u64) = (1, 3650);
const DEFAULT_AUDIT_RETENTION_DAYS: u64 = 365;
const AUDIT_RETENTION_DAYS_RANGE: (u64, u64) = (30, 3650);
const DEFAULT_DEVICE_ACCESS_TTL_SECS: u64 = 900;
const DEVICE_ACCESS_TTL_SECS_RANGE: (u64, u64) = (60, 3600);
const DEFAULT_DEVICE_IDLE_DAYS: u64 = 30;
const DEVICE_IDLE_DAYS_RANGE: (u64, u64) = (1, 90);
const DEFAULT_DEVICE_ABSOLUTE_DAYS: u64 = 90;
const DEVICE_ABSOLUTE_DAYS_RANGE: (u64, u64) = (1, 365);
const DEFAULT_VERIFY_BYTES_PER_RUN: u64 = 1024 * 1024 * 1024;
const VERIFY_BYTES_PER_RUN_RANGE: (u64, u64) = (1024 * 1024, 1024 * 1024 * 1024 * 1024);

/// Keys reserved for later milestones. They are rejected explicitly instead
/// of being silently ignored.
const RESERVED_KEYS: [&str; 4] = [
    "DATABASE_URL",
    "STORAGE_BACKEND",
    "STORAGE_ROOT",
    "SESSION_SECRET_FILE",
];

const SUPPORTED_KEYS: &[&str] = &[
    "BIND",
    "PUBLIC_URL",
    "TRUSTED_PROXIES",
    "MAX_BODY_BYTES",
    "REQUEST_TIMEOUT_SECS",
    "SHUTDOWN_GRACE_SECS",
    "LOG_FORMAT",
    "BOOTSTRAP_SECRET_FILE",
    "SESSION_IDLE_TIMEOUT_MINUTES",
    "SESSION_ABSOLUTE_TIMEOUT_HOURS",
    "TRASH_RETENTION_DAYS",
    "STORAGE_DIR",
    "MAX_FILE_SIZE_BYTES",
    "UPLOAD_TTL_HOURS",
    "DATABASE_URL_FILE",
    "DATABASE_MAX_CONNECTIONS",
    "DATABASE_CONNECT_TIMEOUT_SECS",
    "MAINTENANCE_INTERVAL_SECS",
    "VERSION_RETENTION_DAYS",
    "VERIFY_BYTES_PER_RUN",
    "QUARANTINE_DAYS",
    "MAINTENANCE_ENABLED",
    "ADMIN_DIR",
    "AUDIT_RETENTION_DAYS",
    "AUDIT_DOWNLOADS",
    "MIGRATE_DATABASE_URL_FILE",
    "DATABASE_RUNTIME_ROLE",
    "METRICS_ADDR",
    "METRICS_TOKEN_FILE",
    "DEVICE_ACCESS_TTL",
    "DEVICE_IDLE_DAYS",
    "DEVICE_ABSOLUTE_DAYS",
    "SECRET_KEY_FILE",
    "SECRET_KEY_OLD_FILE",
    "FIGCRAFT_URL",
    "FIGCRAFT_PUBLIC_URL",
    "FIGCRAFT_KEY_FILE",
    "FIGCRAFT_ALLOW_INSECURE_INTERNAL",
    "MAIL_TRANSPORT",
    "SMTP_HOST",
    "SMTP_PORT",
    "SMTP_TLS",
    "SMTP_USERNAME",
    "SMTP_PASSWORD_FILE",
    "SMTP_FROM",
    "SMTP_AUTH",
    "SMTP_OAUTH_PROVIDER",
    "SMTP_OAUTH_TENANT_ID",
    "SMTP_OAUTH_CLIENT_ID",
    "SMTP_OAUTH_CERT_FILE",
    "SMTP_OAUTH_KEY_FILE",
    "SMTP_OAUTH_CLIENT_SECRET_FILE",
    "SMTP_OAUTH_SERVICE_ACCOUNT_FILE",
    "GRAPH_TENANT_ID",
    "GRAPH_CLIENT_ID",
    "GRAPH_CERT_FILE",
    "GRAPH_KEY_FILE",
    "GRAPH_CLIENT_SECRET_FILE",
    "GRAPH_SENDER",
    "GRAPH_SAVE_TO_SENT",
];

/// Mail delivery settings. Secrets are read only by server startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailSettings {
    Smtp {
        host: String,
        port: u16,
        tls: craftcloud_mail::SmtpTls,
        username: Option<String>,
        password_file: Option<PathBuf>,
        from: String,
    },
    Graph {
        tenant_id: String,
        client_id: String,
        certificate_file: Option<PathBuf>,
        key_file: Option<PathBuf>,
        client_secret_file: Option<PathBuf>,
        sender: String,
        save_to_sent: bool,
    },
    SmtpOAuthMicrosoft {
        host: String,
        port: u16,
        tls: craftcloud_mail::SmtpTls,
        username: String,
        from: String,
        tenant_id: String,
        client_id: String,
        certificate_file: Option<PathBuf>,
        key_file: Option<PathBuf>,
        client_secret_file: Option<PathBuf>,
    },
    SmtpOAuthGoogle {
        host: String,
        port: u16,
        tls: craftcloud_mail::SmtpTls,
        username: String,
        from: String,
        service_account_file: PathBuf,
    },
}

/// Optional FigCraft companion service. Secrets are loaded only at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FigcraftSettings {
    pub internal_url: String,
    pub public_url: String,
    pub key_file: PathBuf,
}

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
    /// Secret file with the URL of the **owner** role that `migrate` uses
    /// (`CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE`). `None`: single-role mode,
    /// `migrate` uses `url_file` and the audit guard is advisory.
    pub migrate_url_file: Option<PathBuf>,
    /// Name of the runtime role that `migrate` grants data privileges to
    /// (`CRAFTCLOUD_DATABASE_RUNTIME_ROLE`); set exactly when `migrate_url_file` is.
    pub runtime_role: Option<String>,
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
    /// Absolute path of the bootstrap token secret file. `None` disables the
    /// one-time instance setup. Requires a configured database.
    pub bootstrap_secret_file: Option<PathBuf>,
    /// Current Base64-encoded 32-byte TOTP encryption key file.
    pub secret_key_file: Option<PathBuf>,
    /// Previous key used only to decrypt secrets during rotation.
    pub secret_key_old_file: Option<PathBuf>,
    /// All three FigCraft settings are required together; absent means disabled.
    pub figcraft: Option<FigcraftSettings>,
    /// Origin (`scheme://host[:port]`) of the public URL, used for the CSRF
    /// Origin check.
    pub public_origin: Option<String>,
    /// Whether the session cookie is `Secure` with the `__Host-` prefix.
    /// False only for an http public URL on a loopback host.
    pub cookie_secure: bool,
    /// Session idle timeout.
    pub session_idle_timeout: Duration,
    /// Session absolute timeout.
    pub session_absolute_timeout: Duration,
    /// Device access-token lifetime, configured in seconds.
    pub device_access_ttl: Duration,
    /// Device-session idle timeout, configured in days.
    pub device_idle_timeout: Duration,
    /// Device-session absolute timeout, configured in days.
    pub device_absolute_timeout: Duration,
    /// How long soft-deleted projects and files stay restorable (ADR-0014).
    pub trash_retention: Duration,
    /// Directory of the local object store, if configured. Validated when the
    /// server starts (exists, writable, no links, same file system).
    pub storage_dir: Option<PathBuf>,
    /// Largest accepted file size in bytes (ADR-0014: 5 GiB by default).
    pub max_file_size_bytes: u64,
    /// How long an upload session stays open.
    pub upload_ttl: Duration,
    /// Time between two runs of the maintenance job (purge, blob garbage
    /// collection, reconciliation, verification).
    pub maintenance_interval: Duration,
    /// How long a superseded file version stays after its successor was
    /// created (ADR-0014: 30 days by default).
    pub version_retention: Duration,
    /// Bytes the maintenance job re-reads per run to verify stored blobs.
    pub verify_bytes_per_run: u64,
    /// How long objects that reconciliation took out of service stay in the
    /// quarantine before they are deleted.
    pub quarantine_retention: Duration,
    /// Whether the maintenance job runs at all (loop and command line).
    /// Switch it off after restoring a database until the pairing with the
    /// object store is verified.
    pub maintenance_enabled: bool,
    /// Directory with the built admin UI (`apps/craftcloud-admin/dist`), served
    /// under `/admin`. `None` serves no UI. Validated when the server starts.
    pub admin_dir: Option<PathBuf>,
    /// Audit events older than this many days are deleted by the maintenance
    /// job (ADR-0016: 365 days by default, at least 30).
    pub audit_retention_days: u32,
    /// Whether file downloads are recorded as audit events.
    pub audit_downloads: bool,
    /// Address of the separate internal listener that serves `GET /metrics`
    /// (`CRAFTCLOUD_METRICS_ADDR`). `None`: metrics are switched off. The
    /// metrics are never served on the public listener.
    pub metrics_addr: Option<SocketAddr>,
    /// Secret file with a bearer token that scrapers must send
    /// (`CRAFTCLOUD_METRICS_TOKEN_FILE`). Required when the listener is not
    /// bound to a loopback address.
    pub metrics_token_file: Option<PathBuf>,
    /// Optional SMTP delivery. `None` preserves link-copy mode.
    pub mail: Option<MailSettings>,
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
        let migrate_url_file = field(&mut errors, env, "MIGRATE_DATABASE_URL_FILE", |key, raw| {
            parse_absolute_path(key, raw)
        });
        let runtime_role = field(&mut errors, env, "DATABASE_RUNTIME_ROLE", |key, raw| {
            let name = raw.trim();
            if craftcloud_db::migrate::valid_role_name(name) {
                Ok(name.to_owned())
            } else {
                Err(ConfigError::new(
                    key,
                    "must be a plain role name (letters, digits and underscore, at most 63 characters)",
                ))
            }
        });
        if migrate_url_file.is_some() != runtime_role.is_some() {
            errors.push(ConfigError::new(
                &format!("{PREFIX}DATABASE_RUNTIME_ROLE"),
                "CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE and CRAFTCLOUD_DATABASE_RUNTIME_ROLE must be set together",
            ));
        }
        if migrate_url_file.is_some() && db_url_file.is_none() {
            errors.push(ConfigError::new(
                &format!("{PREFIX}MIGRATE_DATABASE_URL_FILE"),
                "requires CRAFTCLOUD_DATABASE_URL_FILE (the runtime role's URL)",
            ));
        }
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

        let bootstrap_secret_file = field(
            &mut errors,
            env,
            "BOOTSTRAP_SECRET_FILE",
            parse_absolute_path,
        );
        let secret_key_file = field(&mut errors, env, "SECRET_KEY_FILE", parse_absolute_path);
        let secret_key_old_file =
            field(&mut errors, env, "SECRET_KEY_OLD_FILE", parse_absolute_path);
        let mail = parse_mail(env, &mut errors);
        if mail.is_some() {
            for (key, present) in [
                ("DATABASE_URL_FILE", db_url_file.is_some()),
                ("SECRET_KEY_FILE", secret_key_file.is_some()),
                ("PUBLIC_URL", public_url.is_some()),
            ] {
                if !present {
                    errors.push(ConfigError::new(
                        &format!("{PREFIX}{key}"),
                        "required when mail delivery is enabled",
                    ));
                }
            }
        }
        if bootstrap_secret_file.is_some() && db_url_file.is_none() {
            errors.push(ConfigError::new(
                &format!("{PREFIX}BOOTSTRAP_SECRET_FILE"),
                "requires CRAFTCLOUD_DATABASE_URL_FILE: instance setup needs a database",
            ));
        }

        let session_idle_minutes = field(
            &mut errors,
            env,
            "SESSION_IDLE_TIMEOUT_MINUTES",
            |key, raw| parse_ranged(key, raw, SESSION_IDLE_MINUTES_RANGE),
        )
        .unwrap_or(DEFAULT_SESSION_IDLE_MINUTES);
        let session_absolute_hours = field(
            &mut errors,
            env,
            "SESSION_ABSOLUTE_TIMEOUT_HOURS",
            |key, raw| parse_ranged(key, raw, SESSION_ABSOLUTE_HOURS_RANGE),
        )
        .unwrap_or(DEFAULT_SESSION_ABSOLUTE_HOURS);
        let device_access_ttl = field(&mut errors, env, "DEVICE_ACCESS_TTL", |key, raw| {
            parse_ranged(key, raw, DEVICE_ACCESS_TTL_SECS_RANGE)
        })
        .unwrap_or(DEFAULT_DEVICE_ACCESS_TTL_SECS);
        let device_idle_days = field(&mut errors, env, "DEVICE_IDLE_DAYS", |key, raw| {
            parse_ranged(key, raw, DEVICE_IDLE_DAYS_RANGE)
        })
        .unwrap_or(DEFAULT_DEVICE_IDLE_DAYS);
        let device_absolute_days = field(&mut errors, env, "DEVICE_ABSOLUTE_DAYS", |key, raw| {
            parse_ranged(key, raw, DEVICE_ABSOLUTE_DAYS_RANGE)
        })
        .unwrap_or(DEFAULT_DEVICE_ABSOLUTE_DAYS);
        let trash_retention_days = field(&mut errors, env, "TRASH_RETENTION_DAYS", |key, raw| {
            parse_ranged(key, raw, TRASH_RETENTION_DAYS_RANGE)
        })
        .unwrap_or(DEFAULT_TRASH_RETENTION_DAYS);
        let storage_dir = field(&mut errors, env, "STORAGE_DIR", |key, raw| {
            let path = PathBuf::from(raw.trim());
            if raw.trim().is_empty() || !path.is_absolute() {
                Err(ConfigError::new(
                    key,
                    "must be an absolute path to a directory",
                ))
            } else {
                Ok(path)
            }
        });
        let max_file_size_bytes = field(&mut errors, env, "MAX_FILE_SIZE_BYTES", |key, raw| {
            parse_ranged(key, raw, MAX_FILE_SIZE_RANGE)
        })
        .unwrap_or(DEFAULT_MAX_FILE_SIZE_BYTES);
        let upload_ttl_hours = field(&mut errors, env, "UPLOAD_TTL_HOURS", |key, raw| {
            parse_ranged(key, raw, UPLOAD_TTL_HOURS_RANGE)
        })
        .unwrap_or(DEFAULT_UPLOAD_TTL_HOURS);
        let maintenance_interval_secs =
            field(&mut errors, env, "MAINTENANCE_INTERVAL_SECS", |key, raw| {
                parse_ranged(key, raw, MAINTENANCE_INTERVAL_SECS_RANGE)
            })
            .unwrap_or(DEFAULT_MAINTENANCE_INTERVAL_SECS);
        let version_retention_days =
            field(&mut errors, env, "VERSION_RETENTION_DAYS", |key, raw| {
                parse_ranged(key, raw, VERSION_RETENTION_DAYS_RANGE)
            })
            .unwrap_or(DEFAULT_VERSION_RETENTION_DAYS);
        let verify_bytes_per_run = field(&mut errors, env, "VERIFY_BYTES_PER_RUN", |key, raw| {
            parse_ranged(key, raw, VERIFY_BYTES_PER_RUN_RANGE)
        })
        .unwrap_or(DEFAULT_VERIFY_BYTES_PER_RUN);
        let quarantine_days = field(&mut errors, env, "QUARANTINE_DAYS", |key, raw| {
            parse_ranged(key, raw, QUARANTINE_DAYS_RANGE)
        })
        .unwrap_or(DEFAULT_QUARANTINE_DAYS);
        let maintenance_enabled =
            field(
                &mut errors,
                env,
                "MAINTENANCE_ENABLED",
                |key, raw| match raw.trim() {
                    "true" => Ok(true),
                    "false" => Ok(false),
                    _ => Err(ConfigError::new(key, "must be `true` or `false`")),
                },
            )
            .unwrap_or(true);
        let audit_retention_days = field(&mut errors, env, "AUDIT_RETENTION_DAYS", |key, raw| {
            parse_ranged(key, raw, AUDIT_RETENTION_DAYS_RANGE)
        })
        .unwrap_or(DEFAULT_AUDIT_RETENTION_DAYS);
        let audit_downloads = field(&mut errors, env, "AUDIT_DOWNLOADS", |key, raw| {
            match raw.trim() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(ConfigError::new(key, "must be `true` or `false`")),
            }
        })
        .unwrap_or(true);
        let metrics_addr = field(&mut errors, env, "METRICS_ADDR", |key, raw| {
            raw.trim().parse::<SocketAddr>().map_err(|err| {
                ConfigError::new(
                    key,
                    format!(
                        "invalid value {raw:?}: {err}; expected <ip>:<port>, e.g. 127.0.0.1:9090"
                    ),
                )
            })
        });
        let metrics_token_file = field(&mut errors, env, "METRICS_TOKEN_FILE", parse_absolute_path);
        match (metrics_addr, &metrics_token_file) {
            (Some(addr), _) if listeners_overlap(addr, bind) => errors.push(ConfigError::new(
                &format!("{PREFIX}METRICS_ADDR"),
                "must not share a port with CRAFTCLOUD_BIND on the same or an unspecified address: metrics are never served on the public listener",
            )),
            (Some(addr), None) if !addr.ip().is_loopback() => errors.push(ConfigError::new(
                &format!("{PREFIX}METRICS_ADDR"),
                "a non-loopback address requires CRAFTCLOUD_METRICS_TOKEN_FILE (or bind 127.0.0.1 and scrape from the host or a sidecar)",
            )),
            (None, Some(_)) => errors.push(ConfigError::new(
                &format!("{PREFIX}METRICS_TOKEN_FILE"),
                "has no effect without CRAFTCLOUD_METRICS_ADDR",
            )),
            _ => {}
        }
        let admin_dir = field(&mut errors, env, "ADMIN_DIR", |key, raw| {
            let path = PathBuf::from(raw.trim());
            if raw.trim().is_empty() || !path.is_absolute() {
                Err(ConfigError::new(
                    key,
                    "must be an absolute path to the built admin UI directory",
                ))
            } else {
                Ok(path)
            }
        });
        if admin_dir.is_some()
            && public_url
                .as_deref()
                .and_then(|raw| Url::parse(raw).ok())
                .is_some_and(|url| !matches!(url.path(), "" | "/"))
        {
            errors.push(ConfigError::new(
                &format!("{PREFIX}ADMIN_DIR"),
                "the admin UI is served at /admin of the origin; CRAFTCLOUD_PUBLIC_URL must not have a path (serve the server at the root of its host name)",
            ));
        }
        if session_idle_minutes > session_absolute_hours * 60 {
            errors.push(ConfigError::new(
                &format!("{PREFIX}SESSION_IDLE_TIMEOUT_MINUTES"),
                "must not exceed CRAFTCLOUD_SESSION_ABSOLUTE_TIMEOUT_HOURS",
            ));
        }
        if device_idle_days > device_absolute_days {
            errors.push(ConfigError::new(
                &format!("{PREFIX}DEVICE_IDLE_DAYS"),
                "must not exceed CRAFTCLOUD_DEVICE_ABSOLUTE_DAYS",
            ));
        }
        let (public_origin, cookie_secure) = public_url
            .as_deref()
            .and_then(|raw| Url::parse(raw).ok())
            .map_or((None, true), |url| {
                (
                    Some(url.origin().ascii_serialization()),
                    url.scheme() != "http",
                )
            });

        let figcraft = parse_figcraft(env, &mut errors);
        if figcraft.is_some() {
            // Four HTTP jobs and the serial worker may each hold a FileGuard
            // connection. Leave one more connection for their database work.
            if db_max_connections < 6 {
                errors.push(ConfigError::new(
                    "CRAFTCLOUD_DATABASE_MAX_CONNECTIONS",
                    "must be at least 6 when the FigCraft companion is configured",
                ));
            }
            for (key, present) in [
                ("SECRET_KEY_FILE", secret_key_file.is_some()),
                ("DATABASE_URL_FILE", db_url_file.is_some()),
                ("STORAGE_DIR", storage_dir.is_some()),
            ] {
                if !present {
                    errors.push(ConfigError::new(
                        &format!("{PREFIX}{key}"),
                        "required when the FigCraft companion is configured",
                    ));
                }
            }
        }

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
                    migrate_url_file,
                    runtime_role,
                },
                bootstrap_secret_file,
                secret_key_file,
                secret_key_old_file,
                figcraft,
                public_origin,
                cookie_secure,
                session_idle_timeout: Duration::from_secs(session_idle_minutes * 60),
                session_absolute_timeout: Duration::from_secs(session_absolute_hours * 3600),
                device_access_ttl: Duration::from_secs(device_access_ttl),
                device_idle_timeout: Duration::from_secs(device_idle_days * 24 * 3600),
                device_absolute_timeout: Duration::from_secs(device_absolute_days * 24 * 3600),
                trash_retention: Duration::from_secs(trash_retention_days * 24 * 3600),
                storage_dir,
                max_file_size_bytes,
                upload_ttl: Duration::from_secs(upload_ttl_hours * 3600),
                maintenance_interval: Duration::from_secs(maintenance_interval_secs),
                version_retention: Duration::from_secs(version_retention_days * 24 * 3600),
                verify_bytes_per_run,
                quarantine_retention: Duration::from_secs(quarantine_days * 24 * 3600),
                maintenance_enabled,
                admin_dir,
                audit_retention_days: u32::try_from(audit_retention_days).unwrap_or(u32::MAX),
                audit_downloads,
                metrics_addr,
                metrics_token_file,
                mail,
            })
        } else {
            Err(errors)
        }
    }
}

fn parse_figcraft(
    env: &BTreeMap<String, String>,
    errors: &mut Vec<ConfigError>,
) -> Option<FigcraftSettings> {
    let names = ["FIGCRAFT_URL", "FIGCRAFT_PUBLIC_URL", "FIGCRAFT_KEY_FILE"];
    if !names
        .iter()
        .any(|name| env.contains_key(&format!("{PREFIX}{name}")))
    {
        return None;
    }
    for name in names {
        if !env.contains_key(&format!("{PREFIX}{name}")) {
            errors.push(ConfigError::new(
                &format!("{PREFIX}{name}"),
                "all three FigCraft settings must be configured together",
            ));
        }
    }
    // Plain ws to a non-loopback host is only for a private container network
    // and needs this explicit opt-in (the compose companion sets it).
    let allow_insecure_internal = field(
        errors,
        env,
        "FIGCRAFT_ALLOW_INSECURE_INTERNAL",
        |key, raw| match raw.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(ConfigError::new(key, "must be `true` or `false`")),
        },
    )
    .unwrap_or(false);
    let internal_url = field(errors, env, "FIGCRAFT_URL", |key, value| {
        parse_figcraft_url(key, value, false, allow_insecure_internal)
    });
    let public_url = field(errors, env, "FIGCRAFT_PUBLIC_URL", |key, value| {
        parse_figcraft_url(key, value, true, false)
    });
    let key_file = field(errors, env, "FIGCRAFT_KEY_FILE", parse_absolute_path);
    Some(FigcraftSettings {
        internal_url: internal_url?,
        public_url: public_url?,
        key_file: key_file?,
    })
}

fn parse_figcraft_url(
    key: &str,
    raw: &str,
    public: bool,
    allow_insecure_internal: bool,
) -> Result<String, ConfigError> {
    let invalid = || {
        ConfigError::new(
            key,
            "must be an absolute ws/wss URL without credentials, query, fragment or whitespace; ws is allowed only on loopback, or for the internal URL with CRAFTCLOUD_FIGCRAFT_ALLOW_INSECURE_INTERNAL=true (value not shown)",
        )
    };
    if raw.len() > 2048 || raw.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let url = Url::parse(raw).map_err(|_| invalid())?;
    let host = url.host().ok_or_else(invalid)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.scheme(), "ws" | "wss")
        || (url.scheme() == "ws"
            && !is_loopback_host(&host)
            && (public || !allow_insecure_internal))
    {
        return Err(invalid());
    }
    Ok(url.to_string())
}

/// Whether two listen addresses would compete for the same socket: the same
/// non-zero port and the same address, or either address is unspecified
/// (`0.0.0.0` or `::`, which also covers the other family on dual-stack hosts).
fn listeners_overlap(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() != 0
        && a.port() == b.port()
        && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

fn parse_absolute_path(key: &str, raw: &str) -> Result<PathBuf, ConfigError> {
    let path = PathBuf::from(raw.trim());
    if raw.trim().is_empty() || !path.is_absolute() {
        Err(ConfigError::new(
            key,
            "must be an absolute path to a secret file",
        ))
    } else {
        Ok(path)
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

fn parse_mail(
    env: &BTreeMap<String, String>,
    errors: &mut Vec<ConfigError>,
) -> Option<MailSettings> {
    let start_errors = errors.len();
    let get = |suffix: &str| env.get(&format!("{PREFIX}{suffix}")).map(|v| v.trim());
    let mail_keys = [
        "SMTP_HOST",
        "SMTP_PORT",
        "SMTP_TLS",
        "SMTP_USERNAME",
        "SMTP_PASSWORD_FILE",
        "SMTP_FROM",
        "SMTP_AUTH",
        "SMTP_OAUTH_PROVIDER",
        "SMTP_OAUTH_TENANT_ID",
        "SMTP_OAUTH_CLIENT_ID",
        "SMTP_OAUTH_CERT_FILE",
        "SMTP_OAUTH_KEY_FILE",
        "SMTP_OAUTH_CLIENT_SECRET_FILE",
        "SMTP_OAUTH_SERVICE_ACCOUNT_FILE",
        "GRAPH_TENANT_ID",
        "GRAPH_CLIENT_ID",
        "GRAPH_CERT_FILE",
        "GRAPH_KEY_FILE",
        "GRAPH_CLIENT_SECRET_FILE",
        "GRAPH_SENDER",
        "GRAPH_SAVE_TO_SENT",
    ];
    let transport = get("MAIL_TRANSPORT");
    if transport.is_none() {
        for suffix in mail_keys {
            if env.contains_key(&format!("{PREFIX}{suffix}")) {
                errors.push(ConfigError::new(
                    &format!("{PREFIX}{suffix}"),
                    "requires an enabled CRAFTCLOUD_MAIL_TRANSPORT",
                ));
            }
        }
        return None;
    }
    let transport = transport.unwrap_or_default();
    if !["smtp", "graph"].contains(&transport) {
        errors.push(ConfigError::new(
            "CRAFTCLOUD_MAIL_TRANSPORT",
            "expected `smtp`, `graph`, or absent",
        ));
        return None;
    }
    let guid = |s: &str| {
        let parts: Vec<&str> = s.split('-').collect();
        parts.len() == 5
            && [8, 4, 4, 4, 12]
                .iter()
                .zip(parts)
                .all(|(n, p)| p.len() == *n && p.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let bare_address = |s: &str| {
        let mut p = s.split('@');
        match (p.next(), p.next(), p.next()) {
            (Some(local), Some(domain), None) => {
                !local.is_empty()
                    && !domain.is_empty()
                    && !s
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control() || ['<', '>'].contains(&c))
            }
            _ => false,
        }
    };
    let required = |suffix: &str, errors: &mut Vec<ConfigError>| -> Option<String> {
        match get(suffix).filter(|s| !s.is_empty()) {
            Some(value) => Some(value.to_owned()),
            None => {
                errors.push(ConfigError::new(&format!("{PREFIX}{suffix}"), "required"));
                None
            }
        }
    };
    let path = |suffix: &str, errors: &mut Vec<ConfigError>| -> Option<PathBuf> {
        match get(suffix) {
            Some(raw) => match parse_absolute_path(&format!("{PREFIX}{suffix}"), raw) {
                Ok(path) => Some(path),
                Err(error) => {
                    errors.push(error);
                    None
                }
            },
            None => None,
        }
    };
    let credential = |prefix: &str,
                      errors: &mut Vec<ConfigError>|
     -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
        let cert_suffix = format!("{prefix}_CERT_FILE");
        let key_suffix = format!("{prefix}_KEY_FILE");
        let secret_suffix = format!("{prefix}_CLIENT_SECRET_FILE");
        let cert = path(&cert_suffix, errors);
        let key = path(&key_suffix, errors);
        let secret = path(&secret_suffix, errors);
        let cert_present = get(&cert_suffix).is_some();
        let key_present = get(&key_suffix).is_some();
        let secret_present = get(&secret_suffix).is_some();
        if cert_present != key_present || (cert_present == secret_present) {
            errors.push(ConfigError::new(&format!("{PREFIX}{cert_suffix}"), "set certificate and key together, or set client secret file; choose exactly one credential"));
        }
        (cert, key, secret)
    };
    if transport == "graph" {
        for suffix in [
            "SMTP_HOST",
            "SMTP_PORT",
            "SMTP_TLS",
            "SMTP_USERNAME",
            "SMTP_PASSWORD_FILE",
            "SMTP_FROM",
            "SMTP_AUTH",
            "SMTP_OAUTH_PROVIDER",
            "SMTP_OAUTH_TENANT_ID",
            "SMTP_OAUTH_CLIENT_ID",
            "SMTP_OAUTH_CERT_FILE",
            "SMTP_OAUTH_KEY_FILE",
            "SMTP_OAUTH_CLIENT_SECRET_FILE",
            "SMTP_OAUTH_SERVICE_ACCOUNT_FILE",
        ] {
            if get(suffix).is_some() {
                errors.push(ConfigError::new(
                    &format!("{PREFIX}{suffix}"),
                    "not valid with graph transport",
                ));
            }
        }
        let tenant = required("GRAPH_TENANT_ID", errors);
        let client = required("GRAPH_CLIENT_ID", errors);
        for (suffix, value) in [
            ("GRAPH_TENANT_ID", tenant.as_deref()),
            ("GRAPH_CLIENT_ID", client.as_deref()),
        ] {
            if value.is_some_and(|v| !guid(v)) {
                errors.push(ConfigError::new(
                    &format!("{PREFIX}{suffix}"),
                    "must be a GUID",
                ));
            }
        }
        let (certificate_file, key_file, client_secret_file) = credential("GRAPH", errors);
        let sender = required("GRAPH_SENDER", errors);
        if sender.as_deref().is_some_and(|v| !bare_address(v)) {
            errors.push(ConfigError::new(
                "CRAFTCLOUD_GRAPH_SENDER",
                "must be a bare email address",
            ));
        }
        let save_to_sent = match get("GRAPH_SAVE_TO_SENT") {
            None => false,
            Some("true") => true,
            Some("false") => false,
            Some(_) => {
                errors.push(ConfigError::new(
                    "CRAFTCLOUD_GRAPH_SAVE_TO_SENT",
                    "must be `true` or `false`",
                ));
                false
            }
        };
        if errors.len() != start_errors {
            return None;
        }
        return Some(MailSettings::Graph {
            tenant_id: tenant?,
            client_id: client?,
            certificate_file,
            key_file,
            client_secret_file,
            sender: sender?,
            save_to_sent,
        });
    }
    for suffix in [
        "GRAPH_TENANT_ID",
        "GRAPH_CLIENT_ID",
        "GRAPH_CERT_FILE",
        "GRAPH_KEY_FILE",
        "GRAPH_CLIENT_SECRET_FILE",
        "GRAPH_SENDER",
        "GRAPH_SAVE_TO_SENT",
    ] {
        if get(suffix).is_some() {
            errors.push(ConfigError::new(
                &format!("{PREFIX}{suffix}"),
                "not valid with smtp transport",
            ));
        }
    }
    let host = required("SMTP_HOST", errors);
    let port = get("SMTP_PORT")
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|p| *p != 0);
    if port.is_none() {
        errors.push(ConfigError::new(
            "CRAFTCLOUD_SMTP_PORT",
            "required; expected a port from 1 to 65535",
        ));
    }
    let tls = match get("SMTP_TLS") {
        Some("starttls") => Some(craftcloud_mail::SmtpTls::StartTls),
        Some("tls") => Some(craftcloud_mail::SmtpTls::ImplicitTls),
        _ => {
            errors.push(ConfigError::new(
                "CRAFTCLOUD_SMTP_TLS",
                "required; expected `starttls` or `tls`",
            ));
            None
        }
    };
    let from = required("SMTP_FROM", errors);
    let auth = get("SMTP_AUTH").unwrap_or("password");
    let provider = get("SMTP_OAUTH_PROVIDER");
    let username = get("SMTP_USERNAME")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(value) = get("SMTP_FROM")
        && !value.contains('<')
        && !bare_address(value)
    {
        errors.push(ConfigError::new(
            "CRAFTCLOUD_SMTP_FROM",
            "must be an email address, optionally with display name",
        ));
    }
    match auth {
        "password" => {
            for suffix in [
                "SMTP_OAUTH_PROVIDER",
                "SMTP_OAUTH_TENANT_ID",
                "SMTP_OAUTH_CLIENT_ID",
                "SMTP_OAUTH_CERT_FILE",
                "SMTP_OAUTH_KEY_FILE",
                "SMTP_OAUTH_CLIENT_SECRET_FILE",
                "SMTP_OAUTH_SERVICE_ACCOUNT_FILE",
            ] {
                if get(suffix).is_some() {
                    errors.push(ConfigError::new(
                        &format!("{PREFIX}{suffix}"),
                        "requires SMTP_AUTH=xoauth2",
                    ));
                }
            }
            let password = path("SMTP_PASSWORD_FILE", errors);
            if username.is_some() != password.is_some()
                || (get("SMTP_AUTH") == Some("password") && username.is_none())
            {
                errors.push(ConfigError::new(
                    "CRAFTCLOUD_SMTP_USERNAME",
                    "SMTP_USERNAME and SMTP_PASSWORD_FILE must be set together",
                ));
            }
            if errors.len() != start_errors {
                return None;
            }
            return Some(MailSettings::Smtp {
                host: host?,
                port: port?,
                tls: tls?,
                username,
                password_file: password,
                from: from?,
            });
        }
        "none" => {
            for suffix in [
                "SMTP_PASSWORD_FILE",
                "SMTP_OAUTH_PROVIDER",
                "SMTP_OAUTH_TENANT_ID",
                "SMTP_OAUTH_CLIENT_ID",
                "SMTP_OAUTH_CERT_FILE",
                "SMTP_OAUTH_KEY_FILE",
                "SMTP_OAUTH_CLIENT_SECRET_FILE",
                "SMTP_OAUTH_SERVICE_ACCOUNT_FILE",
            ] {
                if get(suffix).is_some() {
                    errors.push(ConfigError::new(
                        &format!("{PREFIX}{suffix}"),
                        "credentials are not valid with SMTP_AUTH=none",
                    ));
                }
            }
            if username.is_some() {
                errors.push(ConfigError::new(
                    "CRAFTCLOUD_SMTP_USERNAME",
                    "requires SMTP_AUTH=password or xoauth2",
                ));
            }
            if errors.len() != start_errors {
                return None;
            }
            return Some(MailSettings::Smtp {
                host: host?,
                port: port?,
                tls: tls?,
                username: None,
                password_file: None,
                from: from?,
            });
        }
        "xoauth2" => {
            if get("SMTP_PASSWORD_FILE").is_some() {
                errors.push(ConfigError::new(
                    "CRAFTCLOUD_SMTP_PASSWORD_FILE",
                    "not valid with SMTP_AUTH=xoauth2",
                ));
            }
            let username = match username {
                Some(v) => Some(v),
                None => {
                    errors.push(ConfigError::new(
                        "CRAFTCLOUD_SMTP_USERNAME",
                        "required for delegated sender",
                    ));
                    None
                }
            };
            if let (Some(user), Some(sender)) = (username.as_deref(), from.as_deref()) {
                let mailbox = sender
                    .rsplit_once('<')
                    .map(|(_, a)| a.trim_end_matches('>').trim())
                    .unwrap_or(sender);
                if !bare_address(user)
                    || !bare_address(mailbox)
                    || !user.eq_ignore_ascii_case(mailbox)
                {
                    errors.push(ConfigError::new(
                        "CRAFTCLOUD_SMTP_USERNAME",
                        "must match SMTP_FROM mailbox",
                    ));
                }
            }
            match provider {
                Some("microsoft") => {
                    if get("SMTP_OAUTH_SERVICE_ACCOUNT_FILE").is_some() {
                        errors.push(ConfigError::new(
                            "CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT_FILE",
                            "not valid with microsoft provider",
                        ));
                    }
                    let tenant = required("SMTP_OAUTH_TENANT_ID", errors);
                    let client = required("SMTP_OAUTH_CLIENT_ID", errors);
                    for (suffix, value) in [
                        ("SMTP_OAUTH_TENANT_ID", tenant.as_deref()),
                        ("SMTP_OAUTH_CLIENT_ID", client.as_deref()),
                    ] {
                        if value.is_some_and(|v| !guid(v)) {
                            errors.push(ConfigError::new(
                                &format!("{PREFIX}{suffix}"),
                                "must be a GUID",
                            ));
                        }
                    }
                    let (certificate_file, key_file, client_secret_file) =
                        credential("SMTP_OAUTH", errors);
                    if errors.len() != start_errors {
                        return None;
                    }
                    return Some(MailSettings::SmtpOAuthMicrosoft {
                        host: host?,
                        port: port?,
                        tls: tls?,
                        username: username?,
                        from: from?,
                        tenant_id: tenant?,
                        client_id: client?,
                        certificate_file,
                        key_file,
                        client_secret_file,
                    });
                }
                Some("google") => {
                    for suffix in [
                        "SMTP_OAUTH_TENANT_ID",
                        "SMTP_OAUTH_CLIENT_ID",
                        "SMTP_OAUTH_CERT_FILE",
                        "SMTP_OAUTH_KEY_FILE",
                        "SMTP_OAUTH_CLIENT_SECRET_FILE",
                    ] {
                        if get(suffix).is_some() {
                            errors.push(ConfigError::new(
                                &format!("{PREFIX}{suffix}"),
                                "not valid with google provider",
                            ));
                        }
                    }
                    let service_account_file = path("SMTP_OAUTH_SERVICE_ACCOUNT_FILE", errors);
                    if service_account_file.is_none()
                        && !env.contains_key("CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT_FILE")
                    {
                        errors.push(ConfigError::new(
                            "CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT_FILE",
                            "required",
                        ));
                    }
                    if errors.len() != start_errors {
                        return None;
                    }
                    return Some(MailSettings::SmtpOAuthGoogle {
                        host: host?,
                        port: port?,
                        tls: tls?,
                        username: username?,
                        from: from?,
                        service_account_file: service_account_file?,
                    });
                }
                _ => {
                    errors.push(ConfigError::new(
                        "CRAFTCLOUD_SMTP_OAUTH_PROVIDER",
                        "required; expected `microsoft` or `google`",
                    ));
                }
            }
        }
        _ => errors.push(ConfigError::new(
            "CRAFTCLOUD_SMTP_AUTH",
            "expected `password`, `none`, or `xoauth2`",
        )),
    }
    None
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
    fn bootstrap_secret_file_requires_database_and_absolute_path() {
        let abs = if cfg!(windows) {
            r"C:\run\secrets\x"
        } else {
            "/run/secrets/x"
        };
        let ok = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE", abs),
            ("CRAFTCLOUD_DATABASE_URL_FILE", abs),
        ]))
        .unwrap();
        assert_eq!(ok.bootstrap_secret_file, Some(PathBuf::from(abs)));
        assert_eq!(
            Config::from_env_map(&BTreeMap::new())
                .unwrap()
                .bootstrap_secret_file,
            None
        );

        let no_db = errors_of(&[("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE", abs)]);
        assert_eq!(keys(&no_db), ["CRAFTCLOUD_BOOTSTRAP_SECRET_FILE"]);
        assert!(no_db[0].message.contains("CRAFTCLOUD_DATABASE_URL_FILE"));

        for bad in ["relative/x", ""] {
            let errors = errors_of(&[
                ("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE", bad),
                ("CRAFTCLOUD_DATABASE_URL_FILE", abs),
            ]);
            assert_eq!(
                keys(&errors),
                ["CRAFTCLOUD_BOOTSTRAP_SECRET_FILE"],
                "{bad:?}"
            );
        }
    }

    #[test]
    fn session_and_cookie_settings() {
        let defaults = Config::from_env_map(&BTreeMap::new()).unwrap();
        assert_eq!(defaults.session_idle_timeout, Duration::from_secs(8 * 3600));
        assert_eq!(
            defaults.session_absolute_timeout,
            Duration::from_secs(7 * 24 * 3600)
        );
        assert_eq!(defaults.device_access_ttl, Duration::from_secs(15 * 60));
        assert_eq!(
            defaults.device_idle_timeout,
            Duration::from_secs(30 * 24 * 3600)
        );
        assert_eq!(
            defaults.device_absolute_timeout,
            Duration::from_secs(90 * 24 * 3600)
        );
        assert_eq!(defaults.public_origin, None);
        assert!(defaults.cookie_secure);

        let https = Config::from_env_map(&env(&[(
            "CRAFTCLOUD_PUBLIC_URL",
            "https://cloud.example.org:8443/cc/",
        )]))
        .unwrap();
        assert_eq!(
            https.public_origin.as_deref(),
            Some("https://cloud.example.org:8443")
        );
        assert!(https.cookie_secure);
        let local =
            Config::from_env_map(&env(&[("CRAFTCLOUD_PUBLIC_URL", "http://127.0.0.1:8080")]))
                .unwrap();
        assert_eq!(
            local.public_origin.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert!(!local.cookie_secure);

        let custom = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_SESSION_IDLE_TIMEOUT_MINUTES", "30"),
            ("CRAFTCLOUD_SESSION_ABSOLUTE_TIMEOUT_HOURS", "24"),
        ]))
        .unwrap();
        assert_eq!(
            defaults.trash_retention,
            Duration::from_secs(30 * 24 * 3600)
        );
        let retention =
            Config::from_env_map(&env(&[("CRAFTCLOUD_TRASH_RETENTION_DAYS", "7")])).unwrap();
        assert_eq!(
            retention.trash_retention,
            Duration::from_secs(7 * 24 * 3600)
        );
        assert_eq!(defaults.upload_ttl, Duration::from_secs(24 * 3600));
        let ttl = Config::from_env_map(&env(&[("CRAFTCLOUD_UPLOAD_TTL_HOURS", "168")])).unwrap();
        assert_eq!(ttl.upload_ttl, Duration::from_secs(168 * 3600));
        for bad in ["0", "169", "x"] {
            let key = "CRAFTCLOUD_UPLOAD_TTL_HOURS";
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        assert_eq!(defaults.maintenance_interval, Duration::from_secs(300));
        assert_eq!(
            defaults.version_retention,
            Duration::from_secs(30 * 24 * 3600)
        );
        assert_eq!(defaults.verify_bytes_per_run, 1024 * 1024 * 1024);
        assert_eq!(
            defaults.quarantine_retention,
            Duration::from_secs(30 * 24 * 3600)
        );
        assert!(defaults.maintenance_enabled);
        let off =
            Config::from_env_map(&env(&[("CRAFTCLOUD_MAINTENANCE_ENABLED", "false")])).unwrap();
        assert!(!off.maintenance_enabled);
        for bad in ["", "yes", "1", "TRUE", "off"] {
            let key = "CRAFTCLOUD_MAINTENANCE_ENABLED";
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        let quarantine =
            Config::from_env_map(&env(&[("CRAFTCLOUD_QUARANTINE_DAYS", "3650")])).unwrap();
        assert_eq!(
            quarantine.quarantine_retention,
            Duration::from_secs(3650 * 24 * 3600)
        );
        for bad in ["0", "3651", "x", "-1"] {
            let key = "CRAFTCLOUD_QUARANTINE_DAYS";
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        // Audit retention (ADR-0016): one year by default, 30 days at least.
        assert_eq!(defaults.audit_retention_days, 365);
        assert!(defaults.audit_downloads);
        let audit = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_AUDIT_RETENTION_DAYS", "30"),
            ("CRAFTCLOUD_AUDIT_DOWNLOADS", "false"),
        ]))
        .unwrap();
        assert_eq!(audit.audit_retention_days, 30);
        assert!(!audit.audit_downloads);
        for (key, bad) in [
            ("CRAFTCLOUD_AUDIT_RETENTION_DAYS", "29"),
            ("CRAFTCLOUD_AUDIT_RETENTION_DAYS", "3651"),
            ("CRAFTCLOUD_AUDIT_RETENTION_DAYS", "1y"),
            ("CRAFTCLOUD_AUDIT_DOWNLOADS", "yes"),
            ("CRAFTCLOUD_AUDIT_DOWNLOADS", ""),
        ] {
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        // Role separation: both keys together, and the runtime URL must exist.
        let abs = if cfg!(windows) {
            "C:/secrets/db"
        } else {
            "/run/secrets/db"
        };
        let roles = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_DATABASE_URL_FILE", abs),
            ("CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE", abs),
            ("CRAFTCLOUD_DATABASE_RUNTIME_ROLE", "craftcloud_app"),
        ]))
        .unwrap();
        assert_eq!(
            roles.database.runtime_role.as_deref(),
            Some("craftcloud_app")
        );
        assert!(roles.database.migrate_url_file.is_some());
        assert!(defaults.database.migrate_url_file.is_none());
        for set in [
            vec![
                ("CRAFTCLOUD_DATABASE_URL_FILE", abs),
                ("CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE", abs),
            ],
            vec![
                ("CRAFTCLOUD_DATABASE_URL_FILE", abs),
                ("CRAFTCLOUD_DATABASE_RUNTIME_ROLE", "app"),
            ],
            vec![
                ("CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE", abs),
                ("CRAFTCLOUD_DATABASE_RUNTIME_ROLE", "app"),
            ],
            vec![
                ("CRAFTCLOUD_DATABASE_URL_FILE", abs),
                ("CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE", abs),
                ("CRAFTCLOUD_DATABASE_RUNTIME_ROLE", "a\"b"),
            ],
            vec![
                ("CRAFTCLOUD_DATABASE_URL_FILE", abs),
                ("CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE", "relative"),
                ("CRAFTCLOUD_DATABASE_RUNTIME_ROLE", "app"),
            ],
        ] {
            assert!(Config::from_env_map(&env(&set)).is_err(), "{set:?}");
        }
        let maintenance = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_MAINTENANCE_INTERVAL_SECS", "10"),
            ("CRAFTCLOUD_VERSION_RETENTION_DAYS", "3650"),
            ("CRAFTCLOUD_VERIFY_BYTES_PER_RUN", "1048576"),
        ]))
        .unwrap();
        assert_eq!(maintenance.maintenance_interval, Duration::from_secs(10));
        assert_eq!(
            maintenance.version_retention,
            Duration::from_secs(3650 * 24 * 3600)
        );
        assert_eq!(maintenance.verify_bytes_per_run, 1_048_576);
        for (key, bad) in [
            ("CRAFTCLOUD_MAINTENANCE_INTERVAL_SECS", "9"),
            ("CRAFTCLOUD_MAINTENANCE_INTERVAL_SECS", "86401"),
            ("CRAFTCLOUD_MAINTENANCE_INTERVAL_SECS", "5m"),
            ("CRAFTCLOUD_VERSION_RETENTION_DAYS", "0"),
            ("CRAFTCLOUD_VERSION_RETENTION_DAYS", "3651"),
            ("CRAFTCLOUD_VERSION_RETENTION_DAYS", "-1"),
            ("CRAFTCLOUD_VERIFY_BYTES_PER_RUN", "1048575"),
            ("CRAFTCLOUD_VERIFY_BYTES_PER_RUN", "1099511627777"),
            ("CRAFTCLOUD_VERIFY_BYTES_PER_RUN", "1GiB"),
        ] {
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        assert_eq!(defaults.storage_dir, None);
        assert_eq!(defaults.max_file_size_bytes, 5 * 1024 * 1024 * 1024);
        let storage = Config::from_env_map(&env(&[
            (
                "CRAFTCLOUD_STORAGE_DIR",
                if cfg!(windows) {
                    "C:\\data\\cc"
                } else {
                    "/var/lib/cc"
                },
            ),
            ("CRAFTCLOUD_MAX_FILE_SIZE_BYTES", "1048576"),
        ]))
        .unwrap();
        assert!(storage.storage_dir.is_some());
        assert_eq!(storage.max_file_size_bytes, 1_048_576);
        for (key, bad) in [
            ("CRAFTCLOUD_STORAGE_DIR", "relative/dir"),
            ("CRAFTCLOUD_STORAGE_DIR", ""),
            ("CRAFTCLOUD_MAX_FILE_SIZE_BYTES", "1048575"),
            ("CRAFTCLOUD_MAX_FILE_SIZE_BYTES", "1099511627777"),
            ("CRAFTCLOUD_MAX_FILE_SIZE_BYTES", "5GiB"),
        ] {
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        for bad in ["0", "3651", "abc", "-1"] {
            let key = "CRAFTCLOUD_TRASH_RETENTION_DAYS";
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        assert_eq!(custom.session_idle_timeout, Duration::from_secs(1800));
        assert_eq!(
            custom.session_absolute_timeout,
            Duration::from_secs(24 * 3600)
        );

        for (key, bad) in [
            ("CRAFTCLOUD_SESSION_IDLE_TIMEOUT_MINUTES", "4"),
            ("CRAFTCLOUD_SESSION_IDLE_TIMEOUT_MINUTES", "4321"),
            ("CRAFTCLOUD_SESSION_ABSOLUTE_TIMEOUT_HOURS", "0"),
            ("CRAFTCLOUD_SESSION_ABSOLUTE_TIMEOUT_HOURS", "721"),
        ] {
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        // Idle longer than the absolute lifetime makes no sense.
        let errors = errors_of(&[
            ("CRAFTCLOUD_SESSION_IDLE_TIMEOUT_MINUTES", "600"),
            ("CRAFTCLOUD_SESSION_ABSOLUTE_TIMEOUT_HOURS", "5"),
        ]);
        assert_eq!(keys(&errors), ["CRAFTCLOUD_SESSION_IDLE_TIMEOUT_MINUTES"]);
    }

    #[test]
    fn device_session_lifetimes_are_bounded_and_ordered() {
        let custom = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_DEVICE_ACCESS_TTL", "3600"),
            ("CRAFTCLOUD_DEVICE_IDLE_DAYS", "60"),
            ("CRAFTCLOUD_DEVICE_ABSOLUTE_DAYS", "180"),
        ]))
        .unwrap();
        assert_eq!(custom.device_access_ttl, Duration::from_secs(3600));
        assert_eq!(
            custom.device_idle_timeout,
            Duration::from_secs(60 * 24 * 3600)
        );
        assert_eq!(
            custom.device_absolute_timeout,
            Duration::from_secs(180 * 24 * 3600)
        );
        for (key, bad) in [
            ("CRAFTCLOUD_DEVICE_ACCESS_TTL", "59"),
            ("CRAFTCLOUD_DEVICE_ACCESS_TTL", "3601"),
            ("CRAFTCLOUD_DEVICE_ACCESS_TTL", "x"),
            ("CRAFTCLOUD_DEVICE_IDLE_DAYS", "0"),
            ("CRAFTCLOUD_DEVICE_IDLE_DAYS", "91"),
            ("CRAFTCLOUD_DEVICE_ABSOLUTE_DAYS", "0"),
            ("CRAFTCLOUD_DEVICE_ABSOLUTE_DAYS", "366"),
        ] {
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
        let errors = errors_of(&[
            ("CRAFTCLOUD_DEVICE_IDLE_DAYS", "31"),
            ("CRAFTCLOUD_DEVICE_ABSOLUTE_DAYS", "30"),
        ]);
        assert_eq!(keys(&errors), ["CRAFTCLOUD_DEVICE_IDLE_DAYS"]);
    }

    #[test]
    fn database_url_error_points_to_file_key() {
        let errors = errors_of(&[("CRAFTCLOUD_DATABASE_URL", "postgres://u:pw0rd@h/db")]);
        assert!(errors[0].message.contains("CRAFTCLOUD_DATABASE_URL_FILE"));
        assert!(!errors[0].to_string().contains("pw0rd"));
    }

    #[test]
    fn metrics_listener_is_off_by_default_and_never_shares_the_public_address() {
        let defaults = Config::from_env_map(&BTreeMap::new()).unwrap();
        assert_eq!(defaults.metrics_addr, None);
        assert_eq!(defaults.metrics_token_file, None);
        // Loopback needs no token.
        let local =
            Config::from_env_map(&env(&[("CRAFTCLOUD_METRICS_ADDR", "127.0.0.1:9090")])).unwrap();
        assert_eq!(local.metrics_addr, Some("127.0.0.1:9090".parse().unwrap()));
        // Anything else needs the token file.
        let token = if cfg!(windows) {
            "C:/secrets/metrics"
        } else {
            "/run/secrets/metrics"
        };
        let remote = Config::from_env_map(&env(&[
            ("CRAFTCLOUD_METRICS_ADDR", "0.0.0.0:9090"),
            ("CRAFTCLOUD_METRICS_TOKEN_FILE", token),
        ]))
        .unwrap();
        assert!(remote.metrics_token_file.is_some());
        // Different ports, or different concrete addresses, are fine.
        for (bind, metrics) in [
            ("0.0.0.0:8080", "127.0.0.1:9090"),
            ("127.0.0.1:8080", "127.0.0.2:8080"),
            ("127.0.0.1:8080", "127.0.0.1:0"),
        ] {
            assert!(
                Config::from_env_map(&env(&[
                    ("CRAFTCLOUD_BIND", bind),
                    ("CRAFTCLOUD_METRICS_ADDR", metrics),
                ]))
                .is_ok(),
                "{bind} {metrics}"
            );
        }
        for pairs in [
            // non-loopback without a token
            vec![("CRAFTCLOUD_METRICS_ADDR", "10.0.0.5:9090")],
            // same socket as the public listener
            vec![
                ("CRAFTCLOUD_BIND", "127.0.0.1:9090"),
                ("CRAFTCLOUD_METRICS_ADDR", "127.0.0.1:9090"),
            ],
            // same port, one side unspecified (either direction, either family)
            vec![
                ("CRAFTCLOUD_BIND", "0.0.0.0:9090"),
                ("CRAFTCLOUD_METRICS_ADDR", "127.0.0.1:9090"),
            ],
            vec![
                ("CRAFTCLOUD_BIND", "127.0.0.1:9090"),
                ("CRAFTCLOUD_METRICS_ADDR", "[::]:9090"),
                ("CRAFTCLOUD_METRICS_TOKEN_FILE", token),
            ],
            vec![
                ("CRAFTCLOUD_BIND", "[::]:9090"),
                ("CRAFTCLOUD_METRICS_ADDR", "127.0.0.1:9090"),
            ],
            // a token without a listener
            vec![("CRAFTCLOUD_METRICS_TOKEN_FILE", token)],
            // malformed values
            vec![("CRAFTCLOUD_METRICS_ADDR", "localhost")],
            vec![("CRAFTCLOUD_METRICS_ADDR", "")],
            vec![
                ("CRAFTCLOUD_METRICS_ADDR", "127.0.0.1:9090"),
                ("CRAFTCLOUD_METRICS_TOKEN_FILE", "relative/path"),
            ],
        ] {
            assert!(!errors_of(&pairs).is_empty(), "{pairs:?}");
        }
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

    #[test]
    fn admin_dir_is_optional_and_must_be_absolute() {
        assert_eq!(Config::from_env_map(&env(&[])).unwrap().admin_dir, None);
        let dir = std::env::temp_dir();
        let value = dir.to_string_lossy().into_owned();
        let config = Config::from_env_map(&env(&[("CRAFTCLOUD_ADMIN_DIR", &value)])).unwrap();
        assert_eq!(config.admin_dir, Some(dir));
        for bad in ["", "   ", "dist", "./dist", "../dist"] {
            let key = "CRAFTCLOUD_ADMIN_DIR";
            assert_eq!(keys(&errors_of(&[(key, bad)])), [key], "{key}={bad}");
        }
    }

    #[test]
    fn admin_dir_needs_a_public_url_without_path() {
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        let key = "CRAFTCLOUD_ADMIN_DIR";
        for ok in ["https://cloud.example.org", "https://cloud.example.org/"] {
            let env = env(&[(key, &dir), ("CRAFTCLOUD_PUBLIC_URL", ok)]);
            assert!(Config::from_env_map(&env).is_ok(), "{ok}");
        }
        let env_with_path = env(&[
            (key, &dir),
            ("CRAFTCLOUD_PUBLIC_URL", "https://cloud.example.org/craft"),
        ]);
        let errors = Config::from_env_map(&env_with_path).unwrap_err();
        assert_eq!(keys(&errors), [key]);
        // Without the UI a path in the public URL stays allowed.
        let without = env(&[("CRAFTCLOUD_PUBLIC_URL", "https://cloud.example.org/craft")]);
        assert!(Config::from_env_map(&without).is_ok());
    }

    #[test]
    fn figcraft_is_optional_but_incomplete_configuration_is_rejected() {
        assert!(Config::from_env_map(&env(&[])).unwrap().figcraft.is_none());
        let errors = errors_of(&[("CRAFTCLOUD_FIGCRAFT_URL", "ws://figcraft:47900/")]);
        assert!(keys(&errors).contains(&"CRAFTCLOUD_FIGCRAFT_PUBLIC_URL"));
        assert!(keys(&errors).contains(&"CRAFTCLOUD_FIGCRAFT_KEY_FILE"));
        let path = std::env::temp_dir().join("synthetic-key");
        let path = path.to_string_lossy();
        let configured = env(&[
            ("CRAFTCLOUD_FIGCRAFT_URL", "ws://figcraft:47900/"),
            ("CRAFTCLOUD_FIGCRAFT_ALLOW_INSECURE_INTERNAL", "true"),
            (
                "CRAFTCLOUD_FIGCRAFT_PUBLIC_URL",
                "wss://cloud.example/figcraft/",
            ),
            ("CRAFTCLOUD_FIGCRAFT_KEY_FILE", &path),
            ("CRAFTCLOUD_SECRET_KEY_FILE", &path),
            ("CRAFTCLOUD_DATABASE_URL_FILE", &path),
            ("CRAFTCLOUD_STORAGE_DIR", &path),
        ]);
        let config = Config::from_env_map(&configured).unwrap();
        assert_eq!(
            config.figcraft.unwrap().public_url,
            "wss://cloud.example/figcraft/"
        );
        for required in ["SECRET_KEY_FILE", "DATABASE_URL_FILE", "STORAGE_DIR"] {
            let mut incomplete = configured.clone();
            incomplete.remove(&format!("CRAFTCLOUD_{required}"));
            assert!(Config::from_env_map(&incomplete).is_err());
        }
        for limit in 1..=6 {
            let mut limited = configured.clone();
            limited.insert(
                "CRAFTCLOUD_DATABASE_MAX_CONNECTIONS".into(),
                limit.to_string(),
            );
            assert_eq!(Config::from_env_map(&limited).is_ok(), limit == 6);
        }
        assert!(
            Config::from_env_map(&env(&[("CRAFTCLOUD_DATABASE_MAX_CONNECTIONS", "1")])).is_ok()
        );
    }

    #[test]
    fn figcraft_urls_preserve_prefix_and_reject_credential_or_transport_leaks() {
        for valid in [
            "wss://cloud.example/figcraft/",
            "ws://localhost:47900/",
            "ws://[::1]:47900/",
        ] {
            assert!(parse_figcraft_url("FIGCRAFT_PUBLIC_URL", valid, true, false).is_ok());
        }
        assert!(parse_figcraft_url("FIGCRAFT_URL", "ws://figcraft:47900/", false, true).is_ok());
        assert!(parse_figcraft_url("FIGCRAFT_URL", "ws://127.0.0.1:47900/", false, false).is_ok());
        assert!(
            parse_figcraft_url("FIGCRAFT_URL", "wss://figcraft.internal/", false, false).is_ok()
        );
        // Without the explicit opt-in a non-loopback ws URL is refused, and the
        // opt-in never relaxes the public URL.
        assert!(parse_figcraft_url("FIGCRAFT_URL", "ws://figcraft:47900/", false, false).is_err());
        assert!(
            parse_figcraft_url("FIGCRAFT_PUBLIC_URL", "ws://cloud.example/", true, true).is_err()
        );
        for bad in [
            "ws://cloud.example/",
            "https://cloud.example/",
            "wss://user:private@cloud.example/",
            "wss://cloud.example/?key=private",
            "wss://cloud.example/#private",
            "wss://cloud.example/with space",
        ] {
            let error = parse_figcraft_url("FIGCRAFT_PUBLIC_URL", bad, true, false).unwrap_err();
            assert!(!error.message.contains("private"));
            assert!(!error.message.contains(bad));
        }
    }

    #[test]
    fn mail_is_disabled_without_transport_and_rejects_orphan_smtp_settings() {
        let defaults = Config::from_env_map(&BTreeMap::new()).unwrap();
        assert!(defaults.mail.is_none());
        let errors = errors_of(&[("CRAFTCLOUD_SMTP_HOST", "smtp.example.org")]);
        assert!(keys(&errors).contains(&"CRAFTCLOUD_SMTP_HOST"));
    }

    #[test]
    fn smtp_requires_tls_and_paired_credentials_and_server_secrets() {
        let errors = errors_of(&[("CRAFTCLOUD_MAIL_TRANSPORT", "smtp")]);
        for key in [
            "CRAFTCLOUD_SMTP_HOST",
            "CRAFTCLOUD_SMTP_PORT",
            "CRAFTCLOUD_SMTP_TLS",
            "CRAFTCLOUD_SMTP_FROM",
        ] {
            assert!(keys(&errors).contains(&key), "missing {key}");
        }
        // Shared service prerequisites are checked once the SMTP settings parse.
        let errors = errors_of(&[
            ("CRAFTCLOUD_MAIL_TRANSPORT", "smtp"),
            ("CRAFTCLOUD_SMTP_HOST", "smtp.example.org"),
            ("CRAFTCLOUD_SMTP_PORT", "587"),
            ("CRAFTCLOUD_SMTP_TLS", "starttls"),
            ("CRAFTCLOUD_SMTP_FROM", "Cloud <mail@example.org>"),
        ]);
        for key in [
            "CRAFTCLOUD_DATABASE_URL_FILE",
            "CRAFTCLOUD_SECRET_KEY_FILE",
            "CRAFTCLOUD_PUBLIC_URL",
        ] {
            assert!(keys(&errors).contains(&key), "missing {key}");
        }
        let errors = errors_of(&[
            ("CRAFTCLOUD_MAIL_TRANSPORT", "smtp"),
            ("CRAFTCLOUD_SMTP_HOST", "smtp.example.org"),
            ("CRAFTCLOUD_SMTP_PORT", "587"),
            ("CRAFTCLOUD_SMTP_TLS", "starttls"),
            ("CRAFTCLOUD_SMTP_USERNAME", "mailer"),
            ("CRAFTCLOUD_SMTP_FROM", "Cloud <mail@example.org>"),
        ]);
        assert!(keys(&errors).contains(&"CRAFTCLOUD_SMTP_USERNAME"));
    }

    #[test]
    fn smtp_configuration_accepts_anonymous_relay_and_tls_modes() {
        for (tls, expected) in [
            ("starttls", craftcloud_mail::SmtpTls::StartTls),
            ("tls", craftcloud_mail::SmtpTls::ImplicitTls),
        ] {
            let config = Config::from_env_map(&env(&[
                ("CRAFTCLOUD_MAIL_TRANSPORT", "smtp"),
                ("CRAFTCLOUD_SMTP_HOST", "smtp.example.org"),
                ("CRAFTCLOUD_SMTP_PORT", "587"),
                ("CRAFTCLOUD_SMTP_TLS", tls),
                ("CRAFTCLOUD_SMTP_FROM", "Cloud <mail@example.org>"),
                ("CRAFTCLOUD_DATABASE_URL_FILE", "C:/secrets/database"),
                ("CRAFTCLOUD_SECRET_KEY_FILE", "C:/secrets/key"),
                ("CRAFTCLOUD_PUBLIC_URL", "https://cloud.example.org"),
            ]))
            .unwrap();
            assert!(
                matches!(config.mail, Some(MailSettings::Smtp { tls, username: None, .. }) if tls == expected)
            );
        }
    }

    #[test]
    fn mail_parser_selects_graph_and_oauth_variants_and_rejects_contradictions() {
        let file = std::env::temp_dir().join("craftcloud-synthetic-credential");
        let file = file.to_string_lossy();
        let prerequisites = [
            ("CRAFTCLOUD_DATABASE_URL_FILE", file.as_ref()),
            ("CRAFTCLOUD_SECRET_KEY_FILE", file.as_ref()),
            ("CRAFTCLOUD_PUBLIC_URL", "https://cloud.example.test"),
        ];
        let graph = [
            ("CRAFTCLOUD_MAIL_TRANSPORT", "graph"),
            (
                "CRAFTCLOUD_GRAPH_TENANT_ID",
                "00000000-0000-0000-0000-000000000001",
            ),
            (
                "CRAFTCLOUD_GRAPH_CLIENT_ID",
                "00000000-0000-0000-0000-000000000002",
            ),
            ("CRAFTCLOUD_GRAPH_CLIENT_SECRET_FILE", file.as_ref()),
            ("CRAFTCLOUD_GRAPH_SENDER", "mailer@example.org"),
        ];
        let relay = [
            ("CRAFTCLOUD_MAIL_TRANSPORT", "smtp"),
            ("CRAFTCLOUD_SMTP_HOST", "smtp.example.org"),
            ("CRAFTCLOUD_SMTP_PORT", "587"),
            ("CRAFTCLOUD_SMTP_TLS", "starttls"),
            ("CRAFTCLOUD_SMTP_FROM", "mailer@example.org"),
        ];
        let google = [
            ("CRAFTCLOUD_SMTP_AUTH", "xoauth2"),
            ("CRAFTCLOUD_SMTP_OAUTH_PROVIDER", "google"),
            ("CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT_FILE", file.as_ref()),
            ("CRAFTCLOUD_SMTP_USERNAME", "mailer@example.org"),
        ];
        let graph_config =
            Config::from_env_map(&env(&[prerequisites.as_slice(), graph.as_slice()].concat()))
                .unwrap();
        assert!(matches!(
            graph_config.mail,
            Some(MailSettings::Graph {
                save_to_sent: false,
                ..
            })
        ));
        let relay_config =
            Config::from_env_map(&env(&[prerequisites.as_slice(), relay.as_slice()].concat()))
                .unwrap();
        assert!(matches!(
            relay_config.mail,
            Some(MailSettings::Smtp { username: None, .. })
        ));
        let google_config = Config::from_env_map(&env(&[
            prerequisites.as_slice(),
            relay.as_slice(),
            google.as_slice(),
        ]
        .concat()))
        .unwrap();
        assert!(matches!(
            google_config.mail,
            Some(MailSettings::SmtpOAuthGoogle { .. })
        ));
        for (base, extra, rejected_key) in [
            (
                graph.as_slice(),
                vec![("CRAFTCLOUD_SMTP_HOST", "smtp.example.org")],
                "CRAFTCLOUD_SMTP_HOST",
            ),
            (
                graph.as_slice(),
                vec![("CRAFTCLOUD_GRAPH_CERT_FILE", file.as_ref())],
                "CRAFTCLOUD_GRAPH_CERT_FILE",
            ),
            (
                graph.as_slice(),
                vec![("CRAFTCLOUD_GRAPH_SAVE_TO_SENT", "invalid")],
                "CRAFTCLOUD_GRAPH_SAVE_TO_SENT",
            ),
            (
                relay.as_slice(),
                vec![("CRAFTCLOUD_SMTP_AUTH", "password")],
                "CRAFTCLOUD_SMTP_USERNAME",
            ),
            (
                relay.as_slice(),
                vec![
                    ("CRAFTCLOUD_SMTP_AUTH", "none"),
                    ("CRAFTCLOUD_SMTP_USERNAME", "mailer@example.org"),
                ],
                "CRAFTCLOUD_SMTP_USERNAME",
            ),
        ] {
            let values = [prerequisites.as_slice(), base, extra.as_slice()].concat();
            let errors = Config::from_env_map(&env(&values)).unwrap_err();
            assert!(
                keys(&errors).contains(&rejected_key),
                "missing expected configuration error"
            );
        }
        for (key, value) in [
            ("CRAFTCLOUD_SMTP_PASSWORD_FILE", file.as_ref()),
            (
                "CRAFTCLOUD_SMTP_OAUTH_CLIENT_ID",
                "00000000-0000-0000-0000-000000000002",
            ),
        ] {
            let extra = [(key, value)];
            let values = [
                prerequisites.as_slice(),
                relay.as_slice(),
                google.as_slice(),
                extra.as_slice(),
            ]
            .concat();
            let errors = Config::from_env_map(&env(&values)).unwrap_err();
            assert!(keys(&errors).contains(&key));
        }
    }
}
