//! Roles of the single CraftCloud image (OPS-02).
//!
//! The first argument selects the role: `serve` (the default), `figcraft`,
//! `migrate`, `backup`, `restore`, `healthcheck` and `all-in-one`. Most roles
//! are the existing commands under a shorter name; this module adds the
//! `figcraft` role (the bundled FigCraft server) and the `all-in-one` role
//! (server and FigCraft server in one container under the supervisor).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::supervisor::{self, ChildSpec, Outcome, Policy};

/// Roles listed in the error message for an unknown first argument.
pub const ROLE_LIST: &str = "serve, figcraft, migrate, backup, restore, healthcheck, all-in-one";

/// Where the FigCraft server binary lives in the image.
#[cfg(unix)]
const DEFAULT_FIGCRAFT_BINARY: &str = "/usr/local/bin/figcraft-server";
/// Configuration file of the `figcraft` role (the Compose file mounts it).
#[cfg(unix)]
const DEFAULT_FIGCRAFT_CONFIG: &str = "/etc/figcraft-server/server.toml";
/// Writable state directory of the `all-in-one` role (a volume in `docker run`).
const DEFAULT_ALL_IN_ONE_DIR: &str = "/var/lib/craftcloud/figcraft";
/// Listener of the bundled FigCraft server in `all-in-one`. It listens on all
/// container interfaces so that the operator can publish it on the host's
/// loopback address for the reverse proxy; the server reaches it over loopback.
const ALL_IN_ONE_FIGCRAFT_LISTEN: &str = "0.0.0.0:47900";
/// Where the CraftCloud server connects to the bundled FigCraft server.
const ALL_IN_ONE_FIGCRAFT_URL: &str = "ws://127.0.0.1:47900/";

/// Rewrites a role name and its arguments to the command line the parser
/// knows: `serve` is the default, `backup ...` is `admin backup ...` and
/// `restore DIR` is `admin backup restore DIR`. Other arguments pass through.
#[must_use]
pub fn expand_role(args: &[String]) -> Vec<String> {
    match args {
        [role] if role == "serve" => Vec::new(),
        [role, rest @ ..] if role == "backup" && !rest.is_empty() => {
            let mut expanded = vec!["admin".to_owned(), "backup".to_owned()];
            expanded.extend(rest.iter().cloned());
            expanded
        }
        [role, rest @ ..] if role == "restore" && !rest.is_empty() => {
            let mut expanded = vec![
                "admin".to_owned(),
                "backup".to_owned(),
                "restore".to_owned(),
            ];
            expanded.extend(rest.iter().cloned());
            expanded
        }
        other => other.to_vec(),
    }
}

/// Starts the FigCraft server in place of this process, so that the container
/// runtime's stop signal reaches it directly.
#[cfg(unix)]
pub fn run_figcraft() -> ExitCode {
    use std::os::unix::process::CommandExt;
    let binary = env_or("CRAFTCLOUD_FIGCRAFT_BIN", DEFAULT_FIGCRAFT_BINARY);
    let config = env_or("CRAFTCLOUD_FIGCRAFT_CONFIG", DEFAULT_FIGCRAFT_CONFIG);
    if !Path::new(&config).is_file() {
        eprintln!("craftcloud-server figcraft: configuration file {config} not found");
        return ExitCode::from(supervisor::EXIT_CONFIG);
    }
    // `exec` only returns when the program could not be started.
    let error = std::process::Command::new(&binary)
        .arg("--config")
        .arg(&config)
        .exec();
    eprintln!("craftcloud-server figcraft: cannot start {binary}: {error}");
    ExitCode::FAILURE
}

/// The FigCraft role needs a Linux container.
#[cfg(not(unix))]
pub fn run_figcraft() -> ExitCode {
    eprintln!("craftcloud-server figcraft: this role is available on Linux only");
    ExitCode::FAILURE
}

/// Runs the server and the bundled FigCraft server in this container.
pub async fn run_all_in_one(stop: impl std::future::Future<Output = ()>) -> ExitCode {
    let state_dir = PathBuf::from(env_or("CRAFTCLOUD_ALL_IN_ONE_DIR", DEFAULT_ALL_IN_ONE_DIR));
    let setup = match prepare_figcraft(&|key| std::env::var(key).ok(), &state_dir) {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("craftcloud-server all-in-one: {message}");
            return ExitCode::from(supervisor::EXIT_CONFIG);
        }
    };
    let own = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("craftcloud-server all-in-one: cannot find its own executable: {err}");
            return ExitCode::FAILURE;
        }
    };
    let specs = vec![
        ChildSpec {
            name: "server",
            program: own.clone(),
            args: vec!["serve".to_owned()],
            envs: setup.server_env.clone(),
        },
        ChildSpec {
            name: "figcraft",
            program: own,
            args: vec!["figcraft".to_owned()],
            envs: vec![(
                "CRAFTCLOUD_FIGCRAFT_CONFIG".to_owned(),
                setup.config_file.to_string_lossy().into_owned(),
            )],
        },
    ];
    tracing::info!("all-in-one: starting the server and the FigCraft server");
    match supervisor::run(specs, Policy::standard(), stop).await {
        Outcome::Stopped => ExitCode::SUCCESS,
        Outcome::Fatal(code) => ExitCode::from(code),
    }
}

/// What the `all-in-one` role prepared for its children.
#[derive(Debug)]
pub struct FigcraftSetup {
    /// Configuration file for the FigCraft server.
    pub config_file: PathBuf,
    /// Environment the server child gets in addition to its own.
    pub server_env: Vec<(String, String)>,
}

/// Writes the FigCraft server's key file and configuration below `state_dir`
/// and returns the environment the CraftCloud server needs to reach it. Needs
/// `CRAFTCLOUD_FIGCRAFT_KEY_FILE` (the same file the server reads) and a
/// public `wss` URL, given as `CRAFTCLOUD_FIGCRAFT_PUBLIC_URL` or derived
/// from `CRAFTCLOUD_PUBLIC_URL`. Never prints the key.
pub fn prepare_figcraft(
    env: &dyn Fn(&str) -> Option<String>,
    state_dir: &Path,
) -> Result<FigcraftSetup, String> {
    let key_path = env("CRAFTCLOUD_FIGCRAFT_KEY_FILE")
        .filter(|value| !value.is_empty())
        .ok_or("CRAFTCLOUD_FIGCRAFT_KEY_FILE is required (a file with 64 random hexadecimal characters)")?;
    let key = std::fs::read_to_string(&key_path)
        .map_err(|err| format!("cannot read CRAFTCLOUD_FIGCRAFT_KEY_FILE: {}", err.kind()))?;
    let key = key.trim();
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(
            "CRAFTCLOUD_FIGCRAFT_KEY_FILE must contain 64 hexadecimal characters".to_owned(),
        );
    }
    let public_url = match env("CRAFTCLOUD_FIGCRAFT_PUBLIC_URL").filter(|value| !value.is_empty()) {
        Some(url) => url,
        None => derive_public_url(env("CRAFTCLOUD_PUBLIC_URL").as_deref())?,
    };
    validate_public_url(&public_url)?;

    let data_dir = state_dir.join("data");
    std::fs::create_dir_all(&data_dir)
        .map_err(|err| format!("cannot create {}: {}", data_dir.display(), err.kind()))?;
    write_private(&data_dir.join("server.key"), key.as_bytes())?;
    let config_file = state_dir.join("server.toml");
    let toml = figcraft_toml(&data_dir, &public_url);
    write_private(&config_file, toml.as_bytes())?;

    let mut server_env = Vec::new();
    for (key, value) in [
        (
            "CRAFTCLOUD_FIGCRAFT_URL",
            ALL_IN_ONE_FIGCRAFT_URL.to_owned(),
        ),
        (
            "CRAFTCLOUD_FIGCRAFT_ALLOW_INSECURE_INTERNAL",
            "true".to_owned(),
        ),
        ("CRAFTCLOUD_FIGCRAFT_PUBLIC_URL", public_url),
    ] {
        // An explicit setting of the operator wins.
        if env(key).is_none_or(|existing| existing.is_empty()) {
            server_env.push((key.to_owned(), value));
        }
    }
    Ok(FigcraftSetup {
        config_file,
        server_env,
    })
}

/// `https://cloud.example.org[:port]` becomes `wss://cloud.example.org[:port]/figcraft/`.
fn derive_public_url(public_url: Option<&str>) -> Result<String, String> {
    let raw = public_url.filter(|value| !value.is_empty()).ok_or(
        "set CRAFTCLOUD_FIGCRAFT_PUBLIC_URL (wss://<host>/figcraft/) or CRAFTCLOUD_PUBLIC_URL",
    )?;
    let parsed =
        url::Url::parse(raw).map_err(|_| "CRAFTCLOUD_PUBLIC_URL is not a valid URL".to_owned())?;
    if parsed.scheme() != "https" {
        return Err("CRAFTCLOUD_PUBLIC_URL must use https to derive the FigCraft URL".to_owned());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "CRAFTCLOUD_PUBLIC_URL has no host".to_owned())?;
    Ok(match parsed.port() {
        Some(port) => format!("wss://{host}:{port}/figcraft/"),
        None => format!("wss://{host}/figcraft/"),
    })
}

/// The value becomes a TOML string: only `wss` URLs with a conservative
/// character set, no credentials, query or fragment (the same rules as
/// `deploy/compose/figcraft/init.sh`).
fn validate_public_url(url: &str) -> Result<(), String> {
    let allowed = |byte: u8| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b':' | b'/' | b'.' | b'_' | b'~' | b'%' | b'[' | b']' | b'-'
            )
    };
    if !url.starts_with("wss://") || url.len() > 512 || !url.bytes().all(allowed) {
        return Err("the FigCraft public URL must be a plain wss:// URL".to_owned());
    }
    Ok(())
}

fn figcraft_toml(data_dir: &Path, public_url: &str) -> String {
    format!(
        "listen = \"{ALL_IN_ONE_FIGCRAFT_LISTEN}\"\n\
         data = \"{}\"\n\
         public_url = \"{public_url}\"\n\
         open_publish = false\n\
         max_plugins = 0\n\
         max_file_mb = 64\n\
         max_files = 1000\n\
         max_links = 1000\n\
         max_connections = 128\n\
         max_people = 64\n",
        data_dir.display()
    )
}

/// Replaces `path` with a file only its owner can read.
fn write_private(path: &Path, content: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let temporary = path.with_extension("new");
    let _ = std::fs::remove_file(&temporary);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|err| format!("cannot write {}: {}", path.display(), err.kind()))?;
    file.write_all(content)
        .map_err(|err| format!("cannot write {}: {}", path.display(), err.kind()))?;
    drop(file);
    std::fs::rename(&temporary, path)
        .map_err(|err| format!("cannot replace {}: {}", path.display(), err.kind()))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("craftcloud-roles-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn roles_expand_to_the_existing_commands() {
        assert!(expand_role(&args(&["serve"])).is_empty());
        assert_eq!(
            expand_role(&args(&["backup", "create", "--out", "/backups/x"])),
            args(&["admin", "backup", "create", "--out", "/backups/x"])
        );
        assert_eq!(
            expand_role(&args(&["restore", "/backups/x"])),
            args(&["admin", "backup", "restore", "/backups/x"])
        );
        // Everything else, including a bare `backup` and unknown names, is left alone.
        assert_eq!(expand_role(&args(&["backup"])), args(&["backup"]));
        assert_eq!(expand_role(&args(&["migrate"])), args(&["migrate"]));
        assert_eq!(expand_role(&args(&["bogus"])), args(&["bogus"]));
    }

    #[test]
    fn derived_url_uses_wss_and_keeps_the_port() {
        assert_eq!(
            derive_public_url(Some("https://cloud.example.org")).as_deref(),
            Ok("wss://cloud.example.org/figcraft/")
        );
        assert_eq!(
            derive_public_url(Some("https://cloud.example.org:8443/")).as_deref(),
            Ok("wss://cloud.example.org:8443/figcraft/")
        );
        assert!(derive_public_url(Some("http://cloud.example.org")).is_err());
        assert!(derive_public_url(Some("not a url")).is_err());
        assert!(derive_public_url(None).is_err());
    }

    #[test]
    fn public_url_rules_match_the_compose_init_script() {
        assert!(validate_public_url("wss://cloud.example.org/figcraft/").is_ok());
        for bad in [
            "ws://cloud.example.org/figcraft/",
            "wss://user@cloud.example.org/figcraft/",
            "wss://cloud.example.org/figcraft/?x=1",
            "wss://cloud.example.org/figcraft/#f",
            "wss://cloud.example.org/\"x",
            "wss://cloud.example.org/ x",
        ] {
            assert!(validate_public_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn prepare_writes_key_and_configuration_and_points_the_server_at_the_local_listener() {
        let dir = scratch("prepare");
        let key_file = dir.join("figcraft.key");
        assert!(std::fs::write(&key_file, format!("{}\n", "ab".repeat(32))).is_ok());
        let state = dir.join("state");
        let key_path = key_file.to_string_lossy().into_owned();
        let env = move |name: &str| match name {
            "CRAFTCLOUD_FIGCRAFT_KEY_FILE" => Some(key_path.clone()),
            "CRAFTCLOUD_PUBLIC_URL" => Some("https://cloud.example.org".to_owned()),
            _ => None,
        };
        let setup = prepare_figcraft(&env, &state);
        assert!(setup.is_ok(), "{setup:?}");
        let Ok(setup) = setup else { return };
        let toml = std::fs::read_to_string(&setup.config_file).unwrap_or_default();
        assert!(toml.contains("listen = \"0.0.0.0:47900\""), "{toml}");
        assert!(toml.contains("public_url = \"wss://cloud.example.org/figcraft/\""));
        assert_eq!(
            std::fs::read_to_string(state.join("data/server.key")).unwrap_or_default(),
            "ab".repeat(32)
        );
        let names: Vec<&str> = setup
            .server_env
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert!(names.contains(&"CRAFTCLOUD_FIGCRAFT_URL"));
        assert!(names.contains(&"CRAFTCLOUD_FIGCRAFT_ALLOW_INSECURE_INTERNAL"));
        // The key never appears in the environment handed to a child.
        assert!(
            setup
                .server_env
                .iter()
                .all(|(_, value)| !value.contains(&"ab".repeat(32)))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prepare_refuses_a_missing_or_malformed_key() {
        let dir = scratch("badkey");
        let state = dir.join("state");
        let none = |_: &str| None;
        assert!(prepare_figcraft(&none, &state).is_err());
        let key_file = dir.join("short.key");
        assert!(std::fs::write(&key_file, "abcd").is_ok());
        let key_path = key_file.to_string_lossy().into_owned();
        let short = move |name: &str| match name {
            "CRAFTCLOUD_FIGCRAFT_KEY_FILE" => Some(key_path.clone()),
            _ => None,
        };
        let error = prepare_figcraft(&short, &state).err().unwrap_or_default();
        assert!(error.contains("64 hexadecimal"), "{error}");
        assert!(!error.contains("abcd"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
