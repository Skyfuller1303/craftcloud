//! CraftCloud server binary: reads configuration, composes the router and
//! serves it until a shutdown signal arrives.
//!
//! Usage: `craftcloud-server` (serve) or `craftcloud-server migrate`.
//!
//! Exit codes: 0 clean shutdown, 1 runtime failure (including a database that
//! is not migrated), 2 invalid configuration or usage.

mod config;
mod secret;
mod server;

use std::process::ExitCode;

use config::{Config, DatabaseSettings, LogFormat};
use craftcloud_api::{ApiConfig, AuthSettings, Services};
use craftcloud_auth::{BootstrapToken, OpaqueToken, PASSWORD_RESET_TTL};
use craftcloud_core::Capabilities;
use craftcloud_core::NormalizedEmail;
use craftcloud_db::PgPool;
use tracing_subscriber::EnvFilter;

const EXIT_CONFIG: u8 = 2;

const USAGE: &str = "usage: craftcloud-server [migrate | admin issue-password-reset --email <address> | admin set-org-owner --org <slug> --email <address>]\n\
    \n\
    (no argument)                          start the HTTP server\n\
    migrate                                apply pending database migrations and exit\n\
    admin issue-password-reset --email E   print a one-hour password reset link for account E\n\
    admin set-org-owner --org SLUG --email E\n\
                                           break glass: make the enabled account E an owner of organization SLUG\n";

/// A failure with the exit code it maps to.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn runtime(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }

    fn config(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_CONFIG,
            message: message.into(),
        }
    }
}

enum Command {
    Serve,
    Migrate,
    IssuePasswordReset { email: String },
    SetOrgOwner { org: String, email: String },
    Help,
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::Serve),
        [one] if one == "migrate" => Ok(Command::Migrate),
        [one] if one == "--help" || one == "-h" => Ok(Command::Help),
        [admin, action, flag, email]
            if admin == "admin" && action == "issue-password-reset" && flag == "--email" =>
        {
            Ok(Command::IssuePasswordReset {
                email: email.clone(),
            })
        }
        [admin, action, f1, org, f2, email]
            if admin == "admin"
                && action == "set-org-owner"
                && f1 == "--org"
                && f2 == "--email" =>
        {
            Ok(Command::SetOrgOwner {
                org: org.clone(),
                email: email.clone(),
            })
        }
        [one] => Err(format!("unknown argument {one:?}")),
        _ => Err("too many arguments".to_owned()),
    }
}

fn init_tracing(format: LogFormat) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(Command::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(command) => command,
        Err(message) => {
            eprintln!("craftcloud-server: {message}\n{USAGE}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let config = match Config::from_process_env() {
        Ok(config) => config,
        Err(errors) => {
            eprintln!(
                "craftcloud-server: invalid configuration ({} problem(s)):",
                errors.len()
            );
            for error in &errors {
                eprintln!("  - {error}");
            }
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    init_tracing(config.log_format);

    let result = match command {
        Command::Migrate => migrate(&config.database).await,
        Command::Serve => run(config).await,
        Command::IssuePasswordReset { email } => issue_password_reset(&config, &email).await,
        Command::SetOrgOwner { org, email } => set_org_owner(&config, &org, &email).await,
        Command::Help => Ok(()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            tracing::error!("{}", failure.message);
            eprintln!("craftcloud-server: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

/// Reads the database URL from its secret file and connects. The URL is held
/// only in the `Secret` wrapper (zeroed on drop) and never logged; the
/// connection library keeps its own copy while connecting.
async fn connect_database(settings: &DatabaseSettings) -> Result<Option<PgPool>, Failure> {
    let Some(path) = &settings.url_file else {
        return Ok(None);
    };
    let secret = secret::load_secret_file(path)
        .map_err(|err| Failure::config(format!("CRAFTCLOUD_DATABASE_URL_FILE: {err}")))?;
    let pool = craftcloud_db::connect(
        secret.expose(),
        settings.max_connections,
        settings.connect_timeout,
    )
    .await
    .map_err(|err| Failure::runtime(err.to_string()))?;
    Ok(Some(pool))
}

async fn migrate(settings: &DatabaseSettings) -> Result<(), Failure> {
    let Some(pool) = connect_database(settings).await? else {
        return Err(Failure::config(
            "migrate requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    let applied = craftcloud_db::migrate::run(&pool)
        .await
        .map_err(|err| Failure::runtime(err.to_string()))?;
    if applied.is_empty() {
        println!("no pending migrations; schema is up to date");
    } else {
        for migration in &applied {
            println!(
                "applied migration {} ({})",
                migration.version, migration.description
            );
        }
        println!("applied {} migration(s)", applied.len());
    }
    pool.close().await;
    Ok(())
}

/// Loads and validates the bootstrap token (at least 32 characters). Errors
/// never contain the token.
fn load_bootstrap_token(config: &Config) -> Result<Option<BootstrapToken>, Failure> {
    let Some(path) = &config.bootstrap_secret_file else {
        return Ok(None);
    };
    let secret = secret::load_secret_file(path)
        .map_err(|err| Failure::config(format!("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE: {err}")))?;
    BootstrapToken::new(secret.expose())
        .map(Some)
        .map_err(|err| Failure::config(format!("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE: {err}")))
}

/// Offline recovery: prints a single-use reset link to stdout. The token is
/// never logged; it only appears on stdout. The password is never an
/// argument; the user sets it through the link.
async fn issue_password_reset(config: &Config, email: &str) -> Result<(), Failure> {
    let Some(public_url) = &config.public_url else {
        return Err(Failure::config(
            "issue-password-reset requires CRAFTCLOUD_PUBLIC_URL to build the link",
        ));
    };
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "issue-password-reset requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|err| Failure::runtime(format!("{err}; run `craftcloud-server migrate` first")))?;
    let account = match NormalizedEmail::parse(email) {
        Ok(email) => craftcloud_db::accounts::find_by_email(&pool, email.as_str())
            .await
            .map_err(|err| Failure::runtime(err.to_string()))?,
        Err(_) => None,
    };
    let Some(account) = account else {
        return Err(Failure::runtime("no account with that e-mail address"));
    };
    let token = OpaqueToken::generate().map_err(|err| Failure::runtime(err.to_string()))?;
    let now = time::OffsetDateTime::now_utc();
    let outcome = craftcloud_db::password_reset::create(
        &pool,
        &craftcloud_db::password_reset::NewResetToken {
            user_id: account.id,
            token_hash: token.hash(),
            created_by: None,
            now,
            expires_at: now + PASSWORD_RESET_TTL,
        },
    )
    .await
    .map_err(|err| Failure::runtime(err.to_string()))?;
    match outcome {
        craftcloud_db::password_reset::IssueOutcome::Issued { revoked_sessions } => {
            eprintln!(
                "Note: {revoked_sessions} active session(s) of this account were revoked and                  earlier unused reset links were voided."
            );
        }
        craftcloud_db::password_reset::IssueOutcome::AccountDisabled => {
            return Err(Failure::runtime(
                "the account is disabled; no reset link was issued",
            ));
        }
        craftcloud_db::password_reset::IssueOutcome::NoSuchUser => {
            return Err(Failure::runtime("no account with that e-mail address"));
        }
    }
    // The link is a credential: it goes to stdout only (never to the log),
    // and stderr carries the warning so it is visible next to the link.
    eprintln!(
        "WARNING: the link below is a secret that grants access to the account.          Hand it over privately; do not paste it into tickets or chat, and run this          command interactively (for example `docker exec -it`) so that no job log          captures stdout."
    );
    println!("Password reset link (valid for 1 hour, usable once):");
    println!("{public_url}/reset-password#token={}", token.expose());
    pool.close().await;
    Ok(())
}

/// Break-glass recovery: makes an existing, enabled account an owner of an
/// organization that lost its enabled owners. Runs on the host with access to
/// the database secret; it is not reachable through the HTTP API. Writes an
/// audit log line (identifiers only) and prints the result to stdout.
async fn set_org_owner(config: &Config, org_slug: &str, email: &str) -> Result<(), Failure> {
    use craftcloud_db::recovery::{SetOwnerOutcome, set_org_owner};

    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "set-org-owner requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|err| Failure::runtime(format!("{err}; run `craftcloud-server migrate` first")))?;
    let Ok(email) = NormalizedEmail::parse(email) else {
        return Err(Failure::runtime("no account with that e-mail address"));
    };
    let outcome = set_org_owner(&pool, org_slug, email.as_str())
        .await
        .map_err(|err| Failure::runtime(err.to_string()))?;
    let (message, org, user) = match outcome {
        SetOwnerOutcome::Added { org_id, user_id } => {
            ("added as owner", Some(org_id), Some(user_id))
        }
        SetOwnerOutcome::Promoted { org_id, user_id } => {
            ("promoted to owner", Some(org_id), Some(user_id))
        }
        SetOwnerOutcome::AlreadyOwner { org_id, user_id } => {
            ("already an owner", Some(org_id), Some(user_id))
        }
        SetOwnerOutcome::OrgNotFound => {
            return Err(Failure::runtime("no organization with that slug"));
        }
        SetOwnerOutcome::UserNotFound => {
            return Err(Failure::runtime("no account with that e-mail address"));
        }
        SetOwnerOutcome::UserDisabled => {
            return Err(Failure::runtime("the account is disabled; enable it first"));
        }
    };
    // Identifiers only: no e-mail address, no name.
    tracing::info!(
        target: "craftcloud::audit",
        actor = "cli",
        action = "org.break_glass_set_owner",
        org = %org.map(|o| o.to_string()).unwrap_or_default(),
        target_id = %user.map(|u| u.to_string()).unwrap_or_default(),
        result = message,
        "audit"
    );
    println!("{message}");
    pool.close().await;
    Ok(())
}

async fn run(config: Config) -> Result<(), Failure> {
    // Validate the bootstrap secret first: a bad file is a configuration
    // error (exit 2) and must stop startup before anything is bound.
    let bootstrap = load_bootstrap_token(&config)?;
    // Migrations are never applied implicitly: only verify the schema.
    let pool = connect_database(&config.database).await?;
    if let Some(pool) = &pool {
        craftcloud_db::migrate::verify(pool).await.map_err(|err| {
            Failure::runtime(format!(
                "{err}; run `craftcloud-server migrate` (back up the database first)"
            ))
        })?;
        tracing::info!("database schema verified");
        if bootstrap.is_some()
            && craftcloud_db::setup::is_completed(pool)
                .await
                .map_err(|err| Failure::runtime(err.to_string()))?
        {
            tracing::warn!(
                "instance setup is already completed; remove the bootstrap secret file \
                 (CRAFTCLOUD_BOOTSTRAP_SECRET_FILE) from this deployment"
            );
        }
    } else {
        tracing::warn!(
            "no database configured (CRAFTCLOUD_DATABASE_URL_FILE unset); running in prototype mode"
        );
    }

    // The object store is validated at startup when configured; storage
    // features themselves arrive with the upload and download routes.
    let _object_store = match &config.storage_dir {
        Some(dir) => {
            let store = craftcloud_storage::LocalStore::open(dir)
                .await
                .map_err(|err| Failure::runtime(err.to_string()))?;
            tracing::info!(
                max_file_size_bytes = config.max_file_size_bytes,
                "object storage directory verified"
            );
            Some(store)
        }
        None => {
            tracing::warn!("CRAFTCLOUD_STORAGE_DIR is not set: file storage is unavailable");
            None
        }
    };

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|err| Failure::runtime(format!("cannot bind {}: {err}", config.bind)))?;
    tracing::warn!(
        address = %config.bind,
        "starting CraftCloud development prototype (not production-ready; no authentication)"
    );
    if pool.is_some() && config.public_origin.is_none() {
        tracing::warn!(
            "CRAFTCLOUD_PUBLIC_URL is not set: browser requests with an Origin header will be rejected"
        );
    }
    let auth_settings = AuthSettings {
        idle_timeout: config.session_idle_timeout,
        absolute_timeout: config.session_absolute_timeout,
        public_url: config.public_url.clone(),
        public_origin: config.public_origin.clone(),
        cookie_secure: config.cookie_secure,
        trash_retention: config.trash_retention,
        ..AuthSettings::default()
    };
    let services = pool
        .as_ref()
        .map(|pool| Services::new(pool.clone(), bootstrap, auth_settings));
    let router = craftcloud_api::router_with_services(
        Capabilities::development_defaults(),
        &ApiConfig {
            max_body_bytes: config.max_body_bytes,
            request_timeout: config.request_timeout,
            trusted_proxies: config.trusted_proxies.clone(),
        },
        services,
    );
    let outcome = server::serve(listener, router, shutdown_signal(), config.shutdown_grace)
        .await
        .map_err(Failure::runtime)?;
    match outcome {
        server::Shutdown::Graceful => tracing::info!("shut down cleanly"),
        server::Shutdown::Forced => {
            tracing::warn!("shut down after grace period; some connections were cut");
        }
    }
    if let Some(pool) = pool {
        pool.close().await;
    }
    Ok(())
}

/// Resolves on Ctrl-C and, on unix, SIGTERM. If a handler cannot be
/// installed, that source is ignored and the error is logged.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!("cannot listen for Ctrl-C: {err}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                tracing::error!("cannot listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn argument_parsing() {
        assert!(matches!(parse_args(&args(&[])), Ok(Command::Serve)));
        assert!(matches!(
            parse_args(&args(&["migrate"])),
            Ok(Command::Migrate)
        ));
        assert!(matches!(parse_args(&args(&["--help"])), Ok(Command::Help)));
        assert!(parse_args(&args(&["nope"])).is_err());
        assert!(parse_args(&args(&["migrate", "extra"])).is_err());
    }
}
