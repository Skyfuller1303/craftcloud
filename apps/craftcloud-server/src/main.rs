//! CraftCloud server binary: reads configuration, composes the router and
//! serves it until a shutdown signal arrives.
//!
//! Usage: `craftcloud-server` (serve), `craftcloud-server migrate` or
//! `craftcloud-server admin ...` (offline tools, including one maintenance run).
//!
//! Exit codes: 0 clean shutdown, 1 runtime failure (including a database that
//! is not migrated), 2 invalid configuration or usage.

mod admin_ui;
mod config;
mod secret;
mod server;

use std::process::ExitCode;

use config::{Config, DatabaseSettings, LogFormat};
use craftcloud_api::maintenance::{
    Binding, Maintenance, MaintenanceSettings, RunOptions, RunOutcome, Trigger,
    check_store_binding, rebind_store,
};
use craftcloud_api::{
    ApiConfig, AuthSettings, Metrics, Services, SystemClock, metrics_router, router_with_shared,
};
use craftcloud_auth::{BootstrapToken, OpaqueToken, PASSWORD_RESET_TTL};
use craftcloud_core::Capabilities;
use craftcloud_core::NormalizedEmail;
use craftcloud_db::PgPool;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

const EXIT_CONFIG: u8 = 2;

const USAGE: &str = "usage: craftcloud-server [migrate | admin issue-password-reset --email <address> | admin set-org-owner --org <slug> --email <address> | admin maintenance --once [--dry-run] | admin storage-bind --force]\n\
    \n\
    (no argument)                          start the HTTP server\n\
    migrate                                apply pending database migrations and exit\n\
    admin issue-password-reset --email E   print a one-hour password reset link for account E\n\
    admin set-org-owner --org SLUG --email E\n\
                                           break glass: make the enabled account E an owner of organization SLUG\n\
    admin maintenance --once [--dry-run]   run the maintenance job once (retention purge, blob garbage collection,\n\
                                           reconciliation, verification) and print counts; --dry-run changes nothing
    admin storage-bind --force             deliberately (re)bind this database and CRAFTCLOUD_STORAGE_DIR to each other\n";

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
    Maintenance { dry_run: bool },
    StorageBind,
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
        [admin, action, once]
            if admin == "admin" && action == "maintenance" && once == "--once" =>
        {
            Ok(Command::Maintenance { dry_run: false })
        }
        [admin, action, once, dry]
            if admin == "admin"
                && action == "maintenance"
                && once == "--once"
                && dry == "--dry-run" =>
        {
            Ok(Command::Maintenance { dry_run: true })
        }
        [admin, action, force]
            if admin == "admin" && action == "storage-bind" && force == "--force" =>
        {
            Ok(Command::StorageBind)
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

    // Crash tests only: arm the process-level crash point from the environment.
    // Compiled out of normal builds.
    #[cfg(feature = "fault-injection")]
    if let Err(message) = craftcloud_storage::arm_crash_point_from_env() {
        eprintln!("craftcloud-server: {message}");
        return ExitCode::from(EXIT_CONFIG);
    }

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
        Command::Maintenance { dry_run } => maintenance_once(&config, dry_run).await,
        Command::StorageBind => storage_bind(&config).await,
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
    connect_url_file(settings, path, "CRAFTCLOUD_DATABASE_URL_FILE")
        .await
        .map(Some)
}

async fn connect_url_file(
    settings: &DatabaseSettings,
    path: &std::path::Path,
    key: &str,
) -> Result<PgPool, Failure> {
    let secret =
        secret::load_secret_file(path).map_err(|err| Failure::config(format!("{key}: {err}")))?;
    let pool = craftcloud_db::connect(
        secret.expose(),
        settings.max_connections,
        settings.connect_timeout,
    )
    .await
    .map_err(|err| Failure::runtime(err.to_string()))?;
    Ok(pool)
}

/// Applies the migrations. With `CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE` (the
/// owner role) the tables belong to that role and the runtime role named by
/// `CRAFTCLOUD_DATABASE_RUNTIME_ROLE` gets only data privileges; without it a
/// single role does everything and the audit log guard is advisory.
async fn migrate(settings: &DatabaseSettings) -> Result<(), Failure> {
    let pool = match (&settings.migrate_url_file, &settings.url_file) {
        (Some(owner), _) => {
            connect_url_file(settings, owner, "CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE").await?
        }
        (None, Some(runtime)) => {
            connect_url_file(settings, runtime, "CRAFTCLOUD_DATABASE_URL_FILE").await?
        }
        (None, None) => {
            return Err(Failure::config(
                "migrate requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
            ));
        }
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
    if let Some(role) = &settings.runtime_role {
        craftcloud_db::migrate::grant_runtime_role(&pool, role)
            .await
            .map_err(|err| match err {
                // A wrong role setting is a configuration problem (exit 2).
                craftcloud_db::migrate::GrantError::InvalidName
                | craftcloud_db::migrate::GrantError::SameAsOwner
                | craftcloud_db::migrate::GrantError::Privileged(_)
                | craftcloud_db::migrate::GrantError::UnknownRole => {
                    Failure::config(format!("CRAFTCLOUD_DATABASE_RUNTIME_ROLE: {err}"))
                }
                other => Failure::runtime(format!("granting the runtime role: {other}")),
            })?;
        println!(
            "runtime role {role} has data privileges only (audit log: no DELETE, no TRUNCATE)"
        );
    } else {
        println!(
            "single-role mode: the audit log guard is advisory (set CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE and CRAFTCLOUD_DATABASE_RUNTIME_ROLE to separate the roles)"
        );
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

/// Loads the bearer token that protects the metrics listener (at least 32
/// characters). Errors never contain the token.
fn load_metrics_token(config: &Config) -> Result<Option<BootstrapToken>, Failure> {
    let Some(path) = &config.metrics_token_file else {
        return Ok(None);
    };
    let secret = secret::load_secret_file(path)
        .map_err(|err| Failure::config(format!("CRAFTCLOUD_METRICS_TOKEN_FILE: {err}")))?;
    BootstrapToken::new(secret.expose())
        .map(Some)
        .map_err(|err| Failure::config(format!("CRAFTCLOUD_METRICS_TOKEN_FILE: {err}")))
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
    // Host access is the authority here: the audit event has no actor.
    let outcome = craftcloud_db::password_reset::create_audited(
        &pool,
        &craftcloud_db::password_reset::NewResetToken {
            user_id: account.id,
            token_hash: token.hash(),
            created_by: None,
            now,
            expires_at: now + PASSWORD_RESET_TTL,
        },
        &craftcloud_db::audit::AuditCtx::system(),
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

fn maintenance_settings(config: &Config) -> MaintenanceSettings {
    let mut settings = MaintenanceSettings::new(
        config.trash_retention,
        config.version_retention,
        config.upload_ttl,
        config.verify_bytes_per_run,
    );
    settings.quarantine_retention = config.quarantine_retention;
    settings.audit_retention_days = config.audit_retention_days;
    settings
}

/// Deliberately pairs the database with the store directory: the identity in
/// the store marker (or a new one, if the store has none) is written to both.
/// Prints the old and new identities. The operator must have verified that
/// this database and this directory belong together; nothing else calls this.
async fn storage_bind(config: &Config) -> Result<(), Failure> {
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "storage-bind requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    let Some(dir) = &config.storage_dir else {
        return Err(Failure::config(
            "storage-bind requires CRAFTCLOUD_STORAGE_DIR to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|err| Failure::runtime(format!("{err}; run `craftcloud-server migrate` first")))?;
    let store = craftcloud_storage::LocalStore::open(dir)
        .await
        .map_err(|err| Failure::runtime(err.to_string()))?;
    let bound = rebind_store(&pool, &store, time::OffsetDateTime::now_utc())
        .await
        .map_err(|err| Failure::runtime(err.to_string()))?;
    fn show<T: std::fmt::Display>(id: Option<T>) -> String {
        id.map_or_else(|| "none".to_owned(), |id| id.to_string())
    }
    println!("database binding was: {}", show(bound.old_database));
    println!("store marker was:     {}", show(bound.old_marker));
    println!(
        "both are now:         {} (epoch {})",
        bound.new, bound.epoch
    );
    tracing::warn!(
        target: "craftcloud::audit",
        action = "storage.rebind",
        old_database = %show(bound.old_database),
        old_marker = %show(bound.old_marker),
        new = %bound.new,
        "audit"
    );
    pool.close().await;
    Ok(())
}

/// Runs the maintenance job once on the host (the same job as the loop inside
/// the server, guarded by the same database lock). Prints counts only. Exit 1
/// when another run is active or a step failed.
async fn maintenance_once(config: &Config, dry_run: bool) -> Result<(), Failure> {
    if !config.maintenance_enabled {
        return Err(Failure::runtime(
            "maintenance is switched off (CRAFTCLOUD_MAINTENANCE_ENABLED=false); nothing was done",
        ));
    }
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "maintenance requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|err| Failure::runtime(format!("{err}; run `craftcloud-server migrate` first")))?;
    let store = match &config.storage_dir {
        Some(dir) => Some(
            craftcloud_storage::LocalStore::open(dir)
                .await
                .map_err(|err| Failure::runtime(err.to_string()))?,
        ),
        None => {
            eprintln!("Note: CRAFTCLOUD_STORAGE_DIR is not set; the storage steps are skipped.");
            None
        }
    };
    let job = Maintenance::new(
        pool.clone(),
        store,
        std::sync::Arc::new(SystemClock),
        maintenance_settings(config),
    );
    let (_keep_alive, stop) = watch::channel(false);
    let outcome = job
        .run_once(
            RunOptions {
                dry_run,
                trigger: Trigger::Cli,
            },
            &stop,
        )
        .await
        .map_err(|err| Failure::runtime(err.to_string()))?;
    pool.close().await;
    match outcome {
        RunOutcome::Skipped => Err(Failure::runtime(
            "another maintenance run is active; nothing was done",
        )),
        RunOutcome::Completed(report) => {
            println!(
                "{}",
                if dry_run {
                    "maintenance dry run (nothing was changed):"
                } else {
                    "maintenance run finished:"
                }
            );
            for (step, result) in &report.steps {
                let counts: Vec<String> = result
                    .counts
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect();
                let counts = if counts.is_empty() {
                    "nothing to do".to_owned()
                } else {
                    counts.join(" ")
                };
                match result.error {
                    Some(code) => println!("  {step}: FAILED ({code}) {counts}"),
                    None => println!("  {step}: {counts}"),
                }
            }
            if report.has_errors() {
                Err(Failure::runtime("one or more maintenance steps failed"))
            } else {
                Ok(())
            }
        }
    }
}

async fn run(config: Config) -> Result<(), Failure> {
    // The admin UI is loaded into memory before anything else is touched
    // (database, object store, maintenance, socket): a bad directory is a
    // configuration error (exit 2) and leaves no side effects behind.
    let admin_ui = match &config.admin_dir {
        Some(dir) => Some(
            admin_ui::load(dir)
                .map_err(|message| Failure::config(format!("CRAFTCLOUD_ADMIN_DIR: {message}")))?,
        ),
        None => None,
    };
    // Validate the bootstrap secret first: a bad file is a configuration
    // error (exit 2) and must stop startup before anything is bound.
    let bootstrap = load_bootstrap_token(&config)?;
    let metrics_token = load_metrics_token(&config)?;
    let metrics = std::sync::Arc::new(Metrics::new());
    // Migrations are never applied implicitly: only verify the schema.
    let pool = connect_database(&config.database).await?;
    if let Some(pool) = &pool {
        craftcloud_db::migrate::verify(pool).await.map_err(|err| {
            Failure::runtime(format!(
                "{err}; run `craftcloud-server migrate` (back up the database first)"
            ))
        })?;
        tracing::info!("database schema verified");
        if craftcloud_db::migrate::audit_guard_is_advisory(pool)
            .await
            .map_err(|err| Failure::runtime(err.to_string()))?
            == Some(true)
        {
            tracing::warn!(
                "the database role of this server owns the audit log table, so the append-only                  guard is advisory (that role can disable its triggers); use separate roles:                  CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE and CRAFTCLOUD_DATABASE_RUNTIME_ROLE"
            );
        }
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
    let object_store = match &config.storage_dir {
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
        max_file_size_bytes: config.max_file_size_bytes,
        upload_ttl: config.upload_ttl,
        audit_downloads: config.audit_downloads,
        ..AuthSettings::default()
    };
    // The maintenance job runs inside the server next to the HTTP stack; its
    // advisory lock keeps a second server or the CLI from running it at once.
    let (stop_maintenance, stop_rx) = watch::channel(false);
    if !config.maintenance_enabled {
        tracing::warn!(
            "maintenance is switched off (CRAFTCLOUD_MAINTENANCE_ENABLED=false): no purge, no garbage collection, no reconciliation"
        );
    }
    let maintenance_task = pool
        .as_ref()
        .filter(|_| config.maintenance_enabled)
        .map(|pool| {
            let job = std::sync::Arc::new(
                Maintenance::new(
                    pool.clone(),
                    object_store.clone(),
                    std::sync::Arc::new(SystemClock),
                    maintenance_settings(&config),
                )
                .with_metrics(std::sync::Arc::clone(&metrics)),
            );
            tracing::info!(
                interval_secs = config.maintenance_interval.as_secs(),
                "maintenance job scheduled"
            );
            tokio::spawn(job.run_loop(config.maintenance_interval, stop_rx))
        });
    // Uploads and downloads use the store only while it belongs to this
    // database; on a mismatch the routes answer 503 STORAGE_UNAVAILABLE and the
    // maintenance job leaves the store alone.
    let serving_store = match (&pool, &object_store) {
        (Some(pool), Some(store)) => {
            match check_store_binding(pool, store, time::OffsetDateTime::now_utc(), true).await {
                Ok(Binding::Created(id)) => {
                    tracing::info!(store = %id, "object store bound to this database");
                    object_store.clone()
                }
                Ok(Binding::Matches(_)) => object_store.clone(),
                Ok(Binding::EpochConflict(conflict)) => {
                    // Serving is safe (it deletes nothing); maintenance will not delete.
                    tracing::error!(
                        ?conflict,
                        "the database and the object store are out of step (restored database?): destructive maintenance is blocked \
                         (verify the pairing, then run `craftcloud-server admin storage-bind --force`)"
                    );
                    object_store.clone()
                }
                Ok(Binding::Mismatch(reason)) => {
                    tracing::error!(
                        ?reason,
                        "the object store does not belong to this database: file storage is disabled \
                         (verify the pairing, then run `craftcloud-server admin storage-bind --force`)"
                    );
                    None
                }
                Err(err) => {
                    return Err(Failure::runtime(format!(
                        "cannot check the store binding: {err}"
                    )));
                }
            }
        }
        _ => object_store.clone(),
    };
    let store_refused = object_store.is_some() && serving_store.is_none();
    let services = pool.as_ref().map(|pool| {
        let services = Services::new(pool.clone(), bootstrap, auth_settings)
            .with_metrics(std::sync::Arc::clone(&metrics));
        let services = match serving_store {
            Some(store) => services.with_storage(store),
            None => services,
        };
        std::sync::Arc::new(if store_refused {
            services.with_storage_refused()
        } else {
            services
        })
    });
    let api_config = ApiConfig {
        max_body_bytes: config.max_body_bytes,
        request_timeout: config.request_timeout,
        trusted_proxies: config.trusted_proxies.clone(),
        metrics: std::sync::Arc::clone(&metrics),
    };
    let router = router_with_shared(
        Capabilities::development_defaults(),
        &api_config,
        services.clone(),
    );
    let router = match admin_ui {
        Some(assets) => {
            tracing::info!("serving the admin UI under /admin");
            router.merge(craftcloud_api::with_request_context(
                admin_ui::router(assets),
                &api_config,
            ))
        }
        None => router,
    };
    // The metrics listener is separate from the public one and off by default.
    let metrics_task = match config.metrics_addr {
        Some(addr) => {
            let metrics_listener = tokio::net::TcpListener::bind(addr).await.map_err(|err| {
                Failure::runtime(format!("cannot bind metrics address {addr}: {err}"))
            })?;
            tracing::info!(
                address = %addr,
                token_required = metrics_token.is_some(),
                "serving GET /metrics on the internal metrics listener"
            );
            let router = metrics_router(
                std::sync::Arc::clone(&metrics),
                services.clone(),
                metrics_token,
            );
            let mut stop = stop_maintenance.subscribe();
            Some(tokio::spawn(server::serve(
                metrics_listener,
                router,
                async move {
                    // Resolves when the main server has ended (or the sender is gone).
                    let _ = stop.wait_for(|stopped| *stopped).await;
                },
                std::time::Duration::from_secs(2),
            )))
        }
        None => None,
    };
    // On the shutdown signal readiness turns 503 at once, so a proxy stops
    // sending new requests while the running ones drain.
    let draining = services.clone();
    let shutdown = async move {
        shutdown_signal().await;
        if let Some(services) = &draining {
            services.begin_draining();
        }
    };
    let outcome = server::serve(listener, router, shutdown, config.shutdown_grace)
        .await
        .map_err(Failure::runtime);
    // Stop the maintenance loop between items; a run in progress finishes its item.
    let _ = stop_maintenance.send(true);
    if let Some(task) = metrics_task
        && tokio::time::timeout(config.shutdown_grace, task)
            .await
            .is_err()
    {
        tracing::warn!("metrics listener did not stop within the grace period");
    }
    if let Some(task) = maintenance_task
        && tokio::time::timeout(config.shutdown_grace, task)
            .await
            .is_err()
    {
        tracing::warn!("maintenance job did not stop within the grace period");
    }
    let outcome = outcome?;
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

    #[test]
    fn maintenance_arguments() {
        assert!(matches!(
            parse_args(&args(&["admin", "maintenance", "--once"])),
            Ok(Command::Maintenance { dry_run: false })
        ));
        assert!(matches!(
            parse_args(&args(&["admin", "maintenance", "--once", "--dry-run"])),
            Ok(Command::Maintenance { dry_run: true })
        ));
        // `--once` is required and the order is fixed.
        assert!(parse_args(&args(&["admin", "maintenance"])).is_err());
        assert!(parse_args(&args(&["admin", "maintenance", "--dry-run"])).is_err());
        assert!(parse_args(&args(&["admin", "maintenance", "--dry-run", "--once"])).is_err());
        assert!(parse_args(&args(&["admin", "maintenance", "--once", "--force"])).is_err());
        assert!(matches!(
            parse_args(&args(&["admin", "storage-bind", "--force"])),
            Ok(Command::StorageBind)
        ));
        // Rebinding is never implicit.
        assert!(parse_args(&args(&["admin", "storage-bind"])).is_err());
        assert!(parse_args(&args(&["admin", "storage-bind", "--yes"])).is_err());
    }
}
