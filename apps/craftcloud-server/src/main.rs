//! CraftCloud server binary: reads configuration, composes the router and
//! serves it until a shutdown signal arrives.
//!
//! Usage: `craftcloud-server` (serve), `craftcloud-server migrate`,
//! `craftcloud-server healthcheck` (container healthcheck) or
//! `craftcloud-server admin ...` (offline tools, including one maintenance run
//! and backup, verify and restore).
//!
//! Exit codes: 0 clean shutdown, 1 runtime failure (including a database that
//! is not migrated), 2 invalid configuration or usage.

mod admin_ui;
mod config;
mod healthcheck;
mod mail_settings;
mod mail_worker;
mod roles;
mod secret;
mod server;
mod supervisor;

use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use zeroize::Zeroize;
use zeroize::Zeroizing;

use config::{ArchiveSettings, Config, DatabaseSettings, LogFormat};
use craftcloud_api::maintenance::{
    Binding, Maintenance, MaintenanceSettings, RunOptions, RunOutcome, Trigger,
    check_existing_store_binding, check_store_binding, rebind_store,
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
const MAX_APP_CATALOG_BYTES: u64 = 256 * 1024;
const VERSION: &str = match option_env!("CRAFTCLOUD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
const GIT_COMMIT: &str = match option_env!("CRAFTCLOUD_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

const USAGE: &str = "usage: craftcloud-server [serve | figcraft | all-in-one | backup <args> | restore <dir> | migrate | healthcheck | archive status | archive recall [--org <uuid>] | admin issue-password-reset --email <address> | admin user reset-2fa <email> | admin set-org-owner --org <slug> --email <address> | admin maintenance --once [--dry-run] | admin storage-bind --force | admin storage rechunk (--file <uuid> | --all) [--dry-run] | admin backup create --out <dir> | admin backup verify <dir> [--manifest-sha256 <hex>] | admin backup restore <dir> [--manifest-sha256 <hex>] | admin backup inventory --confirm-complete <dir>... | admin backup release --id <uuid> --manifest-sha256 <hex> --confirm-deleted]\n\
    \n\
    (no argument), serve                   start the HTTP server\n\
    figcraft                               run the bundled FigCraft server in place of this process\n\
    all-in-one                             run the server and the FigCraft server in one container\n\
    backup ARGS, restore DIR               short for `admin backup ARGS` and `admin backup restore DIR`\n\
    migrate                                apply pending database migrations and exit\n\
    healthcheck                            exit 0 if GET /health/ready on the local listener answers 200 (container healthcheck)\n\
    archive status                         show archive configuration and durable database counters\n\
    archive recall [--org UUID]             restore archived blobs to local storage safely\n\
    admin issue-password-reset --email E   print a one-hour password reset link for account E\n\
    admin user reset-2fa E                 clear TOTP and recovery codes; revoke all sessions\n\
    admin set-org-owner --org SLUG --email E\n\
                                           break glass: make the enabled account E an owner of organization SLUG\n\
    admin maintenance --once [--dry-run]   run the maintenance job once (retention purge, blob garbage collection,\n\
                                           reconciliation, verification) and print counts; --dry-run changes nothing
    admin storage-bind --force             deliberately (re)bind this database and CRAFTCLOUD_STORAGE_DIR to each other\n\
    admin storage rechunk (--file UUID | --all) [--dry-run]\n\
                                           safely convert committed whole versions to local chunks; dry run changes nothing\n\
    admin backup create --out DIR          consistent backup of the database and all committed blobs into the new or empty DIR\n\
    admin backup verify DIR                check every checksum of a backup (needs no database; --manifest-sha256 pins the manifest)\n\
    admin backup restore DIR               load a verified backup into an EMPTY database and an EMPTY CRAFTCLOUD_STORAGE_DIR\n\
    admin backup inventory --confirm-complete DIR...\n\
                                           verify supplied backups, pin archive objects, explicitly open archive GC\n\
    admin backup release --id UUID --manifest-sha256 HEX --confirm-deleted\n\
                                           release pins after deleting that exact backup\n";

fn read_app_catalog<R: Read>(reader: R) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_APP_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_APP_CATALOG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "app catalog exceeds maximum size",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "app catalog is not UTF-8"))
}

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
    Healthcheck,
    Figcraft,
    AllInOne,
    ArchiveStatus,
    ArchiveRecall {
        org_id: Option<uuid::Uuid>,
    },
    IssuePasswordReset {
        email: String,
    },
    ResetTwoFactor {
        email: String,
    },
    SetOrgOwner {
        org: String,
        email: String,
    },
    Maintenance {
        dry_run: bool,
    },
    StorageBind,
    StorageRechunk {
        file_id: Option<uuid::Uuid>,
        dry_run: bool,
    },
    BackupCreate {
        out: String,
    },
    BackupVerify {
        dir: String,
        digest: Option<String>,
    },
    BackupRestore {
        dir: String,
        digest: Option<String>,
    },
    BackupInventory {
        dirs: Vec<String>,
    },
    BackupRelease {
        backup_id: String,
        digest: String,
    },
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let args = roles::expand_role(args);
    match args.as_slice() {
        [] => Ok(Command::Serve),
        [one] if one == "migrate" => Ok(Command::Migrate),
        [one] if one == "healthcheck" => Ok(Command::Healthcheck),
        [one] if one == "figcraft" => Ok(Command::Figcraft),
        [one] if one == "all-in-one" => Ok(Command::AllInOne),
        [archive, action] if archive == "archive" && action == "status" => {
            Ok(Command::ArchiveStatus)
        }
        [archive, action] if archive == "archive" && action == "recall" => {
            Ok(Command::ArchiveRecall { org_id: None })
        }
        [archive, action, flag, raw_id]
            if archive == "archive" && action == "recall" && flag == "--org" =>
        {
            let org_id = uuid::Uuid::parse_str(raw_id).map_err(|_| {
                "archive recall --org requires a valid organization UUID".to_owned()
            })?;
            Ok(Command::ArchiveRecall {
                org_id: Some(org_id),
            })
        }
        [one] if one == "--help" || one == "-h" => Ok(Command::Help),
        [one] if one == "--version" || one == "-V" => Ok(Command::Version),
        [admin, action, flag, email]
            if admin == "admin" && action == "issue-password-reset" && flag == "--email" =>
        {
            Ok(Command::IssuePasswordReset {
                email: email.clone(),
            })
        }
        [admin, user, action, email]
            if admin == "admin" && user == "user" && action == "reset-2fa" =>
        {
            Ok(Command::ResetTwoFactor {
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
        [admin, storage, rechunk, flag, raw_id, dry]
            if admin == "admin"
                && storage == "storage"
                && rechunk == "rechunk"
                && flag == "--file"
                && dry == "--dry-run" =>
        {
            let file_id = uuid::Uuid::parse_str(raw_id)
                .map_err(|_| "admin storage rechunk --file requires a valid UUID".to_owned())?;
            Ok(Command::StorageRechunk {
                file_id: Some(file_id),
                dry_run: true,
            })
        }
        [admin, storage, rechunk, flag, raw_id]
            if admin == "admin"
                && storage == "storage"
                && rechunk == "rechunk"
                && flag == "--file" =>
        {
            let file_id = uuid::Uuid::parse_str(raw_id)
                .map_err(|_| "admin storage rechunk --file requires a valid UUID".to_owned())?;
            Ok(Command::StorageRechunk {
                file_id: Some(file_id),
                dry_run: false,
            })
        }
        [admin, storage, rechunk, all, dry]
            if admin == "admin"
                && storage == "storage"
                && rechunk == "rechunk"
                && all == "--all"
                && dry == "--dry-run" =>
        {
            Ok(Command::StorageRechunk {
                file_id: None,
                dry_run: true,
            })
        }
        [admin, storage, rechunk, all]
            if admin == "admin"
                && storage == "storage"
                && rechunk == "rechunk"
                && all == "--all" =>
        {
            Ok(Command::StorageRechunk {
                file_id: None,
                dry_run: false,
            })
        }
        [admin, backup, create, flag, out]
            if admin == "admin" && backup == "backup" && create == "create" && flag == "--out" =>
        {
            Ok(Command::BackupCreate { out: out.clone() })
        }
        [admin, backup, verify, dir]
            if admin == "admin" && backup == "backup" && verify == "verify" =>
        {
            Ok(Command::BackupVerify {
                dir: dir.clone(),
                digest: None,
            })
        }
        [admin, backup, verify, dir, flag, digest]
            if admin == "admin"
                && backup == "backup"
                && verify == "verify"
                && flag == "--manifest-sha256" =>
        {
            Ok(Command::BackupVerify {
                dir: dir.clone(),
                digest: Some(digest.clone()),
            })
        }
        [admin, backup, restore, dir]
            if admin == "admin" && backup == "backup" && restore == "restore" =>
        {
            Ok(Command::BackupRestore {
                dir: dir.clone(),
                digest: None,
            })
        }
        [admin, backup, restore, dir, flag, digest]
            if admin == "admin"
                && backup == "backup"
                && restore == "restore"
                && flag == "--manifest-sha256" =>
        {
            Ok(Command::BackupRestore {
                dir: dir.clone(),
                digest: Some(digest.clone()),
            })
        }
        [admin, backup, inventory, confirm, dirs @ ..]
            if admin == "admin"
                && backup == "backup"
                && inventory == "inventory"
                && confirm == "--confirm-complete" =>
        {
            Ok(Command::BackupInventory {
                dirs: dirs.to_vec(),
            })
        }
        [
            admin,
            backup,
            release,
            id_flag,
            backup_id,
            digest_flag,
            digest,
            confirm,
        ] if admin == "admin"
            && backup == "backup"
            && release == "release"
            && id_flag == "--id"
            && digest_flag == "--manifest-sha256"
            && confirm == "--confirm-deleted" =>
        {
            Ok(Command::BackupRelease {
                backup_id: backup_id.clone(),
                digest: digest.clone(),
            })
        }
        [one] => Err(format!(
            "unknown argument {one:?}; roles: {}",
            roles::ROLE_LIST
        )),
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

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("craftcloud-server: cannot initialize async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async_main());
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}

async fn async_main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(Command::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("craftcloud-server {VERSION} (commit {GIT_COMMIT})");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Healthcheck) => {
            // Before the configuration: the check only needs CRAFTCLOUD_BIND
            // and must not read secrets or log.
            return match healthcheck::run(std::env::var("CRAFTCLOUD_BIND").ok().as_deref()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(message) => {
                    eprintln!("craftcloud-server healthcheck: {message}");
                    ExitCode::FAILURE
                }
            };
        }
        Ok(Command::Figcraft) => return roles::run_figcraft(),
        Ok(Command::AllInOne) => {
            init_tracing(
                if std::env::var("CRAFTCLOUD_LOG_FORMAT").is_ok_and(|format| format == "json") {
                    LogFormat::Json
                } else {
                    LogFormat::Text
                },
            );
            return roles::run_all_in_one(shutdown_signal()).await;
        }
        Ok(Command::BackupVerify { dir, digest }) => {
            // Offline: reads the backup directory only. No configuration,
            // no database, no secrets.
            return match backup_verify(&dir, digest.as_deref()).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(failure) => {
                    eprintln!("craftcloud-server: {}", failure.message);
                    ExitCode::from(failure.code)
                }
            };
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
        Command::ArchiveStatus => archive_status(&config).await,
        Command::ArchiveRecall { org_id } => archive_recall(&config, org_id).await,
        Command::Serve => run(config).await,
        Command::IssuePasswordReset { email } => issue_password_reset(&config, &email).await,
        Command::ResetTwoFactor { email } => reset_two_factor(&config, &email).await,
        Command::SetOrgOwner { org, email } => set_org_owner(&config, &org, &email).await,
        Command::Maintenance { dry_run } => maintenance_once(&config, dry_run).await,
        Command::StorageBind => storage_bind(&config).await,
        Command::StorageRechunk { file_id, dry_run } => {
            if dry_run {
                storage_rechunk_dry_run(&config, file_id).await
            } else {
                storage_rechunk_apply(&config, file_id).await
            }
        }
        Command::BackupCreate { out } => backup_create(&config, &out).await,
        Command::BackupRestore { dir, digest } => {
            backup_restore(&config, &dir, digest.as_deref()).await
        }
        Command::BackupInventory { dirs } => backup_inventory(&config, &dirs).await,
        Command::BackupRelease { backup_id, digest } => {
            backup_release(&config, &backup_id, &digest).await
        }
        Command::Help
        | Command::Version
        | Command::Healthcheck
        | Command::Figcraft
        | Command::AllInOne
        | Command::BackupVerify { .. } => Ok(()),
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

async fn archive_status(config: &Config) -> Result<(), Failure> {
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "archive status requires CRAFTCLOUD_DATABASE_URL_FILE",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|_| Failure::runtime("cannot verify database schema for archive status"))?;
    let status = craftcloud_db::maintenance::archive_status(&pool, config.archive_after)
        .await
        .map_err(|_| Failure::runtime("cannot read archive status"))?;
    println!("archive_configured={}", config.archive.is_some());
    println!("local_bytes={}", status.local_bytes);
    println!("archive_bytes={}", status.archive_bytes);
    println!("pending_archives={}", status.pending_archives);
    println!("archive_errors={}", status.archive_errors);
    println!("pending_archive_deletes={}", status.pending_archive_deletes);
    println!("archive_delete_errors={}", status.archive_delete_errors);
    println!(
        "last_verified_at={}",
        status
            .last_verified_at
            .map_or_else(|| "never".to_owned(), |time| time.to_string())
    );
    println!(
        "last_error_code={}",
        status.last_error_code.as_deref().unwrap_or("none")
    );
    Ok(())
}

async fn audit_archive_configuration(
    pool: &craftcloud_db::PgPool,
    config: &Config,
) -> Result<(), Failure> {
    let current = config.archive.as_ref().map(|settings| {
        let (provider, host) = settings.audit_endpoint();
        let fingerprint: [u8; 32] = Sha256::digest(format!("{settings:?}").as_bytes()).into();
        (provider, host, fingerprint)
    });
    let previous = sqlx::query_as::<_, (bool, Option<String>, Option<String>, Option<String>)>(
        "SELECT (details->>'configured')::boolean, details->>'provider', \
                details->>'endpoint_host', details->>'fingerprint' \
         FROM audit_events WHERE action LIKE 'archive.configuration.%' \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|_| Failure::runtime("cannot read previous archive configuration audit"))?;

    let transition = match (&previous, &current) {
        (None, Some(_)) => Some(("archive.configuration.enabled", true)),
        (Some((false, ..)), Some(_)) => Some(("archive.configuration.enabled", true)),
        (Some((true, _, _, old)), Some((_, _, fingerprint)))
            if old.as_deref() != Some(hex_digest(fingerprint).as_str()) =>
        {
            Some(("archive.configuration.changed", true))
        }
        (Some((true, ..)), None) => Some(("archive.configuration.disabled", false)),
        _ => None,
    };
    if let Some((action, configured)) = transition {
        let (provider, host, fingerprint) = match (&current, &previous) {
            (Some((provider, host, fingerprint)), _) => (*provider, host.clone(), *fingerprint),
            (None, Some((_, Some(provider), Some(host), Some(fingerprint)))) => (
                provider.as_str(),
                host.clone(),
                parse_hex_digest(fingerprint)
                    .ok_or_else(|| Failure::runtime("previous archive audit is invalid"))?,
            ),
            _ => ("none", String::new(), [0; 32]),
        };
        let mut event = craftcloud_db::audit::NewEvent::new(action)
            .detail("configured", craftcloud_db::audit::Detail::Flag(configured))
            .detail(
                "provider",
                craftcloud_db::audit::Detail::Code(match provider {
                    "s3" => "s3",
                    "sftp" => "sftp",
                    "local_test" => "local_test",
                    _ => "none",
                }),
            )
            .detail(
                "fingerprint",
                craftcloud_db::audit::Detail::Fingerprint(fingerprint),
            );
        if !host.is_empty() {
            event = event.detail(
                "endpoint_host",
                craftcloud_db::audit::Detail::EndpointHost(host),
            );
        }
        craftcloud_db::audit::record(pool, &craftcloud_db::audit::AuditCtx::system(), &event)
            .await
            .map_err(|_| Failure::runtime("cannot write archive configuration audit"))?;
    }
    Ok(())
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn parse_hex_digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

fn archive_audit_endpoint(config: &Config) -> Option<(&'static str, String)> {
    if let Some(settings) = &config.archive {
        return Some(settings.audit_endpoint());
    }
    #[cfg(feature = "fault-injection")]
    if std::env::var_os("CC_FAULT_ARCHIVE_LOCAL_DIR").is_some() {
        return Some(("local_test", "localhost".to_owned()));
    }
    None
}

async fn archive_recall(config: &Config, org_id: Option<uuid::Uuid>) -> Result<(), Failure> {
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "archive recall requires CRAFTCLOUD_DATABASE_URL_FILE",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|_| Failure::runtime("cannot verify database schema for archive recall"))?;
    let archive = archive_runtime(config)
        .await?
        .ok_or_else(|| Failure::config("archive recall requires a configured archive provider"))?;
    let (audit_provider, audit_endpoint_host) = archive_audit_endpoint(config)
        .ok_or_else(|| Failure::config("archive recall requires a configured archive provider"))?;
    let store_dir = config
        .storage_dir
        .as_ref()
        .ok_or_else(|| Failure::config("archive recall requires CRAFTCLOUD_STORAGE_DIR"))?;
    let Some(lock) = craftcloud_db::maintenance::try_lock(&pool)
        .await
        .map_err(|_| Failure::runtime("cannot acquire archive maintenance lock"))?
    else {
        return Err(Failure::runtime(
            "archive recall is busy with backup, maintenance, or another recall",
        ));
    };
    let local = craftcloud_storage::LocalStore::open(store_dir)
        .await
        .map_err(|_| Failure::runtime("cannot open local object storage for archive recall"))?;
    match check_existing_store_binding(&pool, &local, time::OffsetDateTime::now_utc()).await {
        Ok(Binding::Matches(_)) => {}
        Ok(_) | Err(_) => {
            return Err(Failure::runtime(
                "archive recall requires the existing database and local store binding to match",
            ));
        }
    }
    let now = time::OffsetDateTime::now_utc();
    let stale_before = now
        .checked_sub(time::Duration::minutes(15))
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let total = craftcloud_db::archive_recall::total_bytes(&pool, org_id, stale_before)
        .await
        .map_err(|_| Failure::runtime("cannot inventory archived objects for recall"))?;
    let required = u64::try_from(total)
        .map_err(|_| Failure::runtime("archive recall inventory has an invalid byte total"))?;
    let available = fs2::available_space(local.root())
        .map_err(|_| Failure::runtime("cannot determine local free space for archive recall"))?;
    if available < required {
        return Err(Failure::runtime(format!(
            "archive recall needs {required} free local bytes; only {available} are available"
        )));
    }

    const PAGE: i64 = 128;
    let mut after = None;
    let mut recalled = 0_u64;
    let mut failed = 0_u64;
    let mut remaining_required = required;
    loop {
        let page = craftcloud_db::archive_recall::candidates_after(
            &pool,
            org_id,
            stale_before,
            after,
            PAGE,
        )
        .await
        .map_err(|_| Failure::runtime("cannot read archived objects for recall"))?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|item| (item.org_id, item.blob_id));
        for candidate in page {
            let needed = u64::try_from(candidate.size_bytes).map_err(|_| {
                Failure::runtime("archive recall inventory has an invalid object size")
            })?;
            let available = fs2::available_space(local.root()).map_err(|_| {
                Failure::runtime("cannot recheck local free space for archive recall")
            })?;
            if available < remaining_required {
                return Err(Failure::runtime(
                    "local free space changed after archive recall preflight; no further objects were claimed",
                ));
            }
            match recall_one(RecallItem {
                pool: &pool,
                local: &local,
                archive: &archive.store,
                crypto: &archive.crypto,
                audit_provider,
                audit_endpoint_host: audit_endpoint_host.clone(),
                org_id: candidate.org_id,
                blob_id: candidate.blob_id,
                now,
                stale_before,
            })
            .await
            {
                Ok(true) => recalled = recalled.saturating_add(1),
                Ok(false) => {}
                Err(()) => failed = failed.saturating_add(1),
            }
            remaining_required = remaining_required.checked_sub(needed).ok_or_else(|| {
                Failure::runtime("archive recall inventory changed after preflight")
            })?;
        }
    }
    let mut recall_event = craftcloud_db::audit::NewEvent::new("archive.recall.operator")
        .result(if failed == 0 {
            craftcloud_db::audit::AuditResult::Success
        } else {
            craftcloud_db::audit::AuditResult::Failed
        })
        .detail(
            "provider",
            craftcloud_db::audit::Detail::Code(audit_provider),
        )
        .detail(
            "endpoint_host",
            craftcloud_db::audit::Detail::EndpointHost(audit_endpoint_host),
        )
        .detail(
            "recalled",
            craftcloud_db::audit::Detail::Count(i64::try_from(recalled).unwrap_or(i64::MAX)),
        )
        .detail(
            "failed",
            craftcloud_db::audit::Detail::Count(i64::try_from(failed).unwrap_or(i64::MAX)),
        );
    if let Some(org_id) = org_id {
        recall_event = recall_event.detail("org_id", craftcloud_db::audit::Detail::Id(org_id));
    }
    let audit_result = craftcloud_db::audit::record(
        &pool,
        &craftcloud_db::audit::AuditCtx::system(),
        &recall_event,
    )
    .await;
    lock.release()
        .await
        .map_err(|_| Failure::runtime("cannot release archive maintenance lock"))?;
    pool.close().await;
    audit_result
        .map_err(|_| Failure::runtime("archive recall completed but audit recording failed"))?;
    println!("recalled={recalled}");
    println!("failed={failed}");
    Ok(())
}

struct RecallItem<'a> {
    pool: &'a PgPool,
    local: &'a craftcloud_storage::LocalStore,
    archive: &'a craftcloud_storage::ArchiveStore,
    crypto: &'a craftcloud_storage::ArchiveCrypto,
    audit_provider: &'static str,
    audit_endpoint_host: String,
    org_id: uuid::Uuid,
    blob_id: uuid::Uuid,
    now: time::OffsetDateTime,
    stale_before: time::OffsetDateTime,
}

async fn recall_one(item: RecallItem<'_>) -> Result<bool, ()> {
    let RecallItem {
        pool,
        local,
        archive,
        crypto,
        audit_provider,
        audit_endpoint_host,
        org_id,
        blob_id,
        now,
        stale_before,
    } = item;
    use craftcloud_storage::{ObjectStore, ReadSource, WriteSink};

    let claim_token = uuid::Uuid::new_v4();
    let Some(mut tx) = craftcloud_db::members::OrgTx::begin(pool, org_id)
        .await
        .map_err(|_| ())?
    else {
        return Ok(false);
    };
    let Some(claim) = tx
        .maint_archive_recall_claim(blob_id, now, stale_before, claim_token)
        .await
        .map_err(|_| ())?
    else {
        tx.commit().await.map_err(|_| ())?;
        return Ok(false);
    };
    tx.commit().await.map_err(|_| ())?;
    craftcloud_storage::crash_point("archive-recall-claimed", 0);

    let result = async {
        let local_key = craftcloud_storage::ObjectKey::parse(&claim.object_key).map_err(|_| ())?;
        let archive_key =
            craftcloud_storage::ObjectKey::parse(&claim.archive_object_key).map_err(|_| ())?;
        let plaintext_len = u64::try_from(claim.size_bytes).map_err(|_| ())?;
        let expected_sha: craftcloud_storage::Sha256Digest =
            claim.sha256.as_slice().try_into().map_err(|_| ())?;
        let envelope = craftcloud_storage::WrappedDataKey::from_bytes(&claim.archive_wrapped_key)
            .map_err(|_| ())?;
        if envelope.blob_id() != blob_id || envelope.plaintext_len() != plaintext_len {
            return Err(());
        }
        let data_key = crypto
            .unwrap_data_key(&envelope, blob_id, plaintext_len)
            .map_err(|_| ())?;

        match local.head(&local_key).await {
            Ok(_) => {
                if local.verify(&local_key, &expected_sha).await.is_ok() {
                    return Ok(());
                }
                return Err(());
            }
            Err(craftcloud_storage::StorageError::NotFound) => {}
            Err(_) => return Err(()),
        }

        let remote_head = archive.head(&archive_key).await.map_err(|_| ())?;
        craftcloud_storage::verify_ciphertext_len(plaintext_len, remote_head.len)
            .map_err(|_| ())?;
        let mut reader = archive
            .open_read(&archive_key, None)
            .await
            .map_err(|_| ())?;
        craftcloud_storage::crash_point("archive-recall-download-started", 0);
        remove_stale_recall_temp(local, &local_key).await?;
        let mut temp = local
            .begin_write_as(local_key.clone(), plaintext_len)
            .await
            .map_err(|_| ())?;
        let mut hasher = Sha256::new();
        let chunk_size = u64::try_from(craftcloud_storage::CHUNK_BYTES).map_err(|_| ())?;
        let blocks = if plaintext_len == 0 {
            1
        } else {
            plaintext_len.checked_add(chunk_size - 1).ok_or(())? / chunk_size
        };
        let mut pending = Vec::with_capacity(craftcloud_storage::CHUNK_BYTES + 16);
        let mut offset = 0_u64;
        for block_index in 0..blocks {
            let plain_block = if plaintext_len == 0 {
                0
            } else {
                (plaintext_len - offset).min(chunk_size)
            };
            let cipher_block =
                usize::try_from(plain_block.checked_add(16).ok_or(())?).map_err(|_| ())?;
            while pending.len() < cipher_block {
                let Some(chunk) = reader.next_chunk().await.map_err(|_| ())? else {
                    return Err(());
                };
                pending.extend_from_slice(&chunk);
            }
            let ciphertext: Vec<u8> = pending.drain(..cipher_block).collect();
            let plaintext = data_key
                .decrypt_block(&envelope, block_index, &ciphertext)
                .map_err(|_| ())?;
            hasher.update(&plaintext);
            temp.write(&plaintext).await.map_err(|_| ())?;
            offset = offset.checked_add(plain_block).ok_or(())?;
        }
        if !pending.is_empty() || reader.remaining() != 0 || offset != plaintext_len {
            return Err(());
        }
        craftcloud_storage::crash_point("archive-recall-download-complete", offset);
        let actual: craftcloud_storage::Sha256Digest = hasher.finalize().into();
        if actual != expected_sha {
            return Err(());
        }
        craftcloud_storage::crash_point("archive-recall-verified", plaintext_len);
        local
            .finalize_as(temp, local_key, plaintext_len, &expected_sha)
            .await
            .map_err(|_| ())?;
        craftcloud_storage::crash_point("archive-recall-local-written", plaintext_len);
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            let Some(mut tx) = craftcloud_db::members::OrgTx::begin(pool, org_id)
                .await
                .map_err(|_| ())?
            else {
                return Err(());
            };
            let finished = tx
                .maint_archive_recall_finish(blob_id, claim_token, now)
                .await
                .map_err(|_| ())?;
            if !finished {
                tx.commit().await.map_err(|_| ())?;
                return Err(());
            }
            tx.audit(
                &craftcloud_db::audit::AuditCtx::system(),
                craftcloud_db::audit::NewEvent::new("archive.recall")
                    .target("blob", blob_id)
                    .detail(
                        "provider",
                        craftcloud_db::audit::Detail::Code(audit_provider),
                    )
                    .detail(
                        "endpoint_host",
                        craftcloud_db::audit::Detail::EndpointHost(audit_endpoint_host.clone()),
                    )
                    .detail(
                        "bytes",
                        craftcloud_db::audit::Detail::Count(claim.size_bytes),
                    ),
            )
            .await
            .map_err(|_| ())?;
            tx.commit().await.map_err(|_| ())?;
            craftcloud_storage::crash_point("archive-recall-committed", 0);
            Ok(true)
        }
        Err(()) => {
            let Some(mut tx) = craftcloud_db::members::OrgTx::begin(pool, org_id)
                .await
                .map_err(|_| ())?
            else {
                return Err(());
            };
            let retry_at = now.checked_add(time::Duration::minutes(5)).unwrap_or(now);
            let retried = tx
                .maint_archive_retry(blob_id, claim_token, "RECALL_FAILED", retry_at, None, None)
                .await
                .map_err(|_| ())?;
            if retried {
                tx.audit(
                    &craftcloud_db::audit::AuditCtx::system(),
                    craftcloud_db::audit::NewEvent::new("archive.recall")
                        .result(craftcloud_db::audit::AuditResult::Failed)
                        .target("blob", blob_id)
                        .detail(
                            "provider",
                            craftcloud_db::audit::Detail::Code(audit_provider),
                        )
                        .detail(
                            "endpoint_host",
                            craftcloud_db::audit::Detail::EndpointHost(audit_endpoint_host.clone()),
                        )
                        .detail(
                            "bytes",
                            craftcloud_db::audit::Detail::Count(claim.size_bytes),
                        ),
                )
                .await
                .map_err(|_| ())?;
            }
            tx.commit().await.map_err(|_| ())?;
            Err(())
        }
    }
}

async fn remove_stale_recall_temp(
    local: &craftcloud_storage::LocalStore,
    key: &craftcloud_storage::ObjectKey,
) -> Result<(), ()> {
    use craftcloud_storage::ObjectStore;

    match local.remove_temp(key.as_str()).await {
        Ok(()) | Err(craftcloud_storage::StorageError::NotFound) => Ok(()),
        Err(_) => Err(()),
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

/// Loads an optional Base64 32-byte TOTP encryption key without making server
/// startup depend on key-file availability. Diagnostics never include paths
/// or key material.
fn load_two_factor_key(path: Option<&std::path::PathBuf>) -> Option<[u8; 32]> {
    let path = path?;
    let secret = match secret::load_secret_file(path) {
        Ok(secret) => secret,
        Err(_) => {
            tracing::warn!("two-factor secret key file is unavailable or invalid");
            return None;
        }
    };
    let decoded = match base64::engine::general_purpose::STANDARD.decode(secret.expose().trim()) {
        Ok(decoded) => decoded,
        Err(_) => {
            tracing::warn!("two-factor secret key is not valid Base64");
            return None;
        }
    };
    let mut decoded = decoded;
    let key = decoded.as_slice().try_into().ok();
    decoded.zeroize();
    if key.is_none() {
        tracing::warn!("two-factor secret key must decode to exactly 32 bytes");
    }
    key
}

struct LoadedFigcraft {
    client: std::sync::Arc<dyn craftcloud_figcraft::Backend>,
    public_url: String,
    internal_url: String,
}

fn load_figcraft(
    config: &Config,
    encryption_available: bool,
) -> Result<Option<LoadedFigcraft>, Failure> {
    let Some(settings) = &config.figcraft else {
        return Ok(None);
    };
    if !encryption_available {
        return Err(Failure::config(
            "FigCraft requires a usable CRAFTCLOUD_SECRET_KEY_FILE; refusing to start without token encryption",
        ));
    }
    let key = secret::load_secret_file(&settings.key_file).map_err(|_| {
        Failure::config("CRAFTCLOUD_FIGCRAFT_KEY_FILE is unavailable or has unsafe permissions")
    })?;
    let client =
        craftcloud_figcraft::Client::new(&settings.internal_url, key.expose().trim().to_owned())
            .map_err(|_| {
                Failure::config("FigCraft endpoint or server key is invalid (value not shown)")
            })?;
    Ok(Some(LoadedFigcraft {
        client: std::sync::Arc::new(client),
        public_url: settings.public_url.clone(),
        internal_url: settings.internal_url.clone(),
    }))
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
            reset_two_factor: false,
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
        craftcloud_db::password_reset::IssueOutcome::EmailRateLimited => {
            return Err(Failure::runtime("email delivery rate limit exceeded"));
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
async fn reset_two_factor(config: &Config, email: &str) -> Result<(), Failure> {
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "reset-2fa requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|err| Failure::runtime(format!("{err}; run `craftcloud-server migrate` first")))?;
    let Ok(email) = NormalizedEmail::parse(email) else {
        return Err(Failure::runtime("no account with that e-mail address"));
    };
    let Some(account) = craftcloud_db::accounts::find_by_email(&pool, email.as_str())
        .await
        .map_err(|err| Failure::runtime(err.to_string()))?
    else {
        return Err(Failure::runtime("no account with that e-mail address"));
    };
    let changed = craftcloud_db::two_factor::reset_for_user(
        &pool,
        account.id,
        None,
        None,
        None,
        None,
        None,
        time::OffsetDateTime::now_utc(),
        &craftcloud_db::audit::AuditCtx::system(),
        "auth.two_factor_reset",
    )
    .await
    .map_err(|err| Failure::runtime(err.to_string()))?;
    if changed {
        println!("Two-factor authentication and recovery codes cleared; all sessions revoked.");
    } else {
        println!("No two-factor enrollment was active.");
    }
    pool.close().await;
    Ok(())
}

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
    settings.version_thinning = craftcloud_db::maintenance::VersionThinningPolicy {
        retention_days: u32::try_from(config.version_retention.as_secs() / 86_400)
            .unwrap_or(u32::MAX),
        keep_all_hours: config.version_keep_all_hours,
        daily_days: config.version_daily_days,
        weekly_weeks: config.version_weekly_weeks,
        max_per_file: config.version_max_per_file,
    };
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

/// Reports committed whole versions selected for rechunking without writes.
async fn storage_rechunk_dry_run(
    config: &Config,
    file_id: Option<uuid::Uuid>,
) -> Result<(), Failure> {
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "storage rechunk requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|err| Failure::runtime(format!("{err}; run `craftcloud-server migrate` first")))?;
    let (whole_versions, chunked_versions, logical_bytes) =
        craftcloud_db::rechunk::inventory(&pool, file_id)
            .await
            .map_err(|_| Failure::runtime("cannot read rechunk preview from the database"))?;
    println!(
        "dry_run=true file={} whole_versions={} already_chunked_versions={} logical_bytes={} changed=false",
        file_id.map_or_else(|| "all".to_owned(), |id| id.to_string()),
        whole_versions,
        chunked_versions,
        logical_bytes
    );
    Ok(())
}

async fn storage_rechunk_apply(
    config: &Config,
    file_id: Option<uuid::Uuid>,
) -> Result<(), Failure> {
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "storage rechunk requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|err| Failure::runtime(format!("{err}; run `craftcloud-server migrate` first")))?;
    let store_dir = config.storage_dir.as_ref().ok_or_else(|| {
        Failure::config("storage rechunk requires CRAFTCLOUD_STORAGE_DIR to be set")
    })?;
    let local = craftcloud_storage::LocalStore::open(store_dir)
        .await
        .map_err(|_| Failure::runtime("cannot open local object storage for rechunk"))?;
    match check_existing_store_binding(&pool, &local, time::OffsetDateTime::now_utc()).await {
        Ok(Binding::Matches(_)) => {}
        Ok(_) | Err(_) => {
            return Err(Failure::runtime(
                "storage rechunk requires the existing database and local store binding to match",
            ));
        }
    }
    let archive = archive_runtime(config).await?;
    let mut key = load_two_factor_key(config.secret_key_file.as_ref()).ok_or_else(|| {
        Failure::config(
            "storage rechunk requires a valid CRAFTCLOUD_SECRET_KEY_FILE with a Base64 32-byte key",
        )
    })?;
    let mut identity_keys = Vec::new();
    if let Some(derived) = craftcloud_storage::derive_chunk_key(&key) {
        identity_keys.push(derived);
    }
    key.zeroize();
    if let Some(mut old_key) = load_two_factor_key(config.secret_key_old_file.as_ref()) {
        if let Some(derived) = craftcloud_storage::derive_chunk_key(&old_key) {
            identity_keys.push(derived);
        }
        old_key.zeroize();
    }
    let archive_access = archive
        .as_ref()
        .map(|runtime| craftcloud_api::rechunk::RechunkArchive {
            store: &runtime.store,
            crypto: &runtime.crypto,
        });
    let result = craftcloud_api::rechunk::run(
        &pool,
        &local,
        file_id,
        false,
        &identity_keys,
        archive_access,
    )
    .await
    .map_err(Failure::runtime);
    identity_keys.zeroize();
    let report = result?;
    println!(
        "dry_run=false file={} whole_versions={} already_chunked_versions={} converted_blobs={} converted_versions={} logical_bytes={} changed={}",
        file_id.map_or_else(|| "all".to_owned(), |id| id.to_string()),
        report.whole_versions,
        report.already_chunked_versions,
        report.converted_blobs,
        report.converted_versions,
        report.logical_bytes,
        report.converted_blobs > 0,
    );
    pool.close().await;
    Ok(())
}

/// How long a backup waits for a running maintenance job before it gives up.
const BACKUP_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

fn backup_failure(error: craftcloud_backup::BackupError) -> Failure {
    Failure::runtime(error.to_string())
}

/// Prints what a verified, created or restored backup contains (counts only).
fn print_backup_summary(manifest: &craftcloud_backup::Manifest) {
    println!(
        "  recovery point:     {} (snapshot time)",
        manifest.created_at
    );
    println!(
        "  made by:            server {} at migration {}",
        manifest.server_version, manifest.migration_version
    );
    println!(
        "  tables:             {} ({} rows)",
        manifest.tables.len(),
        manifest.tables.iter().map(|t| t.rows).sum::<u64>()
    );
    println!(
        "  blobs:              {} ({} bytes)",
        manifest.blobs.count, manifest.blobs.total_bytes
    );
}

/// Offline check of every checksum of a backup directory.
async fn backup_verify(dir: &str, digest: Option<&str>) -> Result<(), Failure> {
    let (dir, digest) = (std::path::PathBuf::from(dir), digest.map(str::to_owned));
    let report =
        tokio::task::spawn_blocking(move || craftcloud_backup::verify(&dir, digest.as_deref()))
            .await
            .map_err(|_| Failure::runtime("the verification task failed"))?
            .map_err(backup_failure)?;
    println!("backup verified: all checksums match");
    print_backup_summary(&report.manifest);
    println!("  manifest sha256:    {}", report.manifest_sha256);
    Ok(())
}

/// Consistent backup of the database and the committed blobs. The manifest
/// digest goes to stdout so that the operator can store it away from the
/// backup (it pins the manifest against tampering); nothing secret is printed.
async fn backup_create(config: &Config, out: &str) -> Result<(), Failure> {
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "backup create requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    let Some(dir) = &config.storage_dir else {
        return Err(Failure::config(
            "backup create requires CRAFTCLOUD_STORAGE_DIR to be set",
        ));
    };
    let store = craftcloud_storage::LocalStore::open(dir)
        .await
        .map_err(|err| Failure::runtime(err.to_string()))?;
    let archive = archive_runtime(config).await?;
    let report = craftcloud_backup::create_with_archive(
        &pool,
        &store,
        std::path::Path::new(out),
        &craftcloud_backup::CreateOptions {
            server_version: env!("CARGO_PKG_VERSION"),
            lock_wait: BACKUP_LOCK_WAIT,
            on_phase: None,
        },
        archive.as_ref().map(|runtime| runtime.access()),
    )
    .await
    .map_err(backup_failure)?;
    pool.close().await;
    println!("backup written to {out}");
    print_backup_summary(&report.manifest);
    println!("  manifest sha256:    {}", report.manifest_sha256);
    println!(
        "  took:               {:.1} s",
        report.duration.as_secs_f64()
    );
    if report.manifest.unavailable_version_blobs > 0 {
        eprintln!(
            "WARNING: {} file version(s) reference blobs that were not committed (for example corrupt); their bytes are not in this backup.",
            report.manifest.unavailable_version_blobs
        );
    }
    eprintln!(
        "Note: the backup holds password hashes, e-mail addresses and all file contents. Encrypt it before it leaves this host."
    );
    if !report.audit_recorded {
        return Err(Failure::runtime(
            "the backup is complete, but its audit event could not be stored",
        ));
    }
    Ok(())
}

/// Restores a backup into an empty database and an empty store directory. Uses
/// the owner role when `CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE` is set (the
/// schema may have to be created, and the audit log keeps its timestamps).
async fn backup_restore(config: &Config, dir: &str, digest: Option<&str>) -> Result<(), Failure> {
    let settings = &config.database;
    let pool = match (&settings.migrate_url_file, &settings.url_file) {
        (Some(owner), _) => {
            connect_url_file(settings, owner, "CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE").await?
        }
        (None, Some(runtime)) => {
            connect_url_file(settings, runtime, "CRAFTCLOUD_DATABASE_URL_FILE").await?
        }
        (None, None) => {
            return Err(Failure::config(
                "backup restore requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
            ));
        }
    };
    let archive = archive_runtime(config).await?;
    craftcloud_backup::preflight_restore_archive(
        std::path::Path::new(dir),
        digest,
        archive.as_ref().map(|runtime| runtime.access()),
    )
    .await
    .map_err(backup_failure)?;
    let Some(store_dir) = &config.storage_dir else {
        return Err(Failure::config(
            "backup restore requires CRAFTCLOUD_STORAGE_DIR to be set",
        ));
    };
    let store = craftcloud_storage::LocalStore::open(store_dir)
        .await
        .map_err(|err| Failure::runtime(err.to_string()))?;
    let report = craftcloud_backup::restore_with_archive(
        &pool,
        &store,
        std::path::Path::new(dir),
        &craftcloud_backup::RestoreOptions {
            expected_manifest_sha256: digest,
            runtime_role: settings.runtime_role.as_deref(),
            on_phase: None,
        },
        archive.as_ref().map(|runtime| runtime.access()),
    )
    .await
    .map_err(backup_failure)?;
    pool.close().await;
    println!("backup restored from {dir}");
    print_backup_summary(&report.manifest);
    println!("  new store identity: {}", report.store_id);
    println!(
        "  restored:           {} rows, {} blobs ({} bytes)",
        report.rows, report.blobs, report.blob_bytes
    );
    println!(
        "  took:               {:.1} s (including verification)",
        report.duration.as_secs_f64()
    );
    println!("All sessions are invalid: everybody has to log in again.");
    if report.aborted_uploads > 0 {
        println!(
            "{} upload(s) that were unfinished at backup time were aborted.",
            report.aborted_uploads
        );
    }
    if report.needs_migration {
        println!(
            "The backup has an older schema than this server: run `craftcloud-server migrate` before starting the server."
        );
    }
    Ok(())
}

async fn backup_inventory(config: &Config, dirs: &[String]) -> Result<(), Failure> {
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "backup inventory requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|error| Failure::runtime(format!("cannot verify database schema: {error}")))?;
    let archive = archive_runtime(config).await?;
    let directories: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
    let report = craftcloud_backup::inventory_backups(
        &pool,
        &directories,
        archive.as_ref().map(|runtime| runtime.access()),
        true,
    )
    .await
    .map_err(backup_failure)?;
    pool.close().await;
    println!("archive backup inventory complete");
    println!("backups={}", report.backups);
    println!("archive_references={}", report.archive_references);
    Ok(())
}

async fn backup_release(config: &Config, backup_id: &str, digest: &str) -> Result<(), Failure> {
    let backup_id = uuid::Uuid::parse_str(backup_id)
        .map_err(|_| Failure::config("--id must be a valid backup UUID"))?;
    let Some(pool) = connect_database(&config.database).await? else {
        return Err(Failure::config(
            "backup release requires CRAFTCLOUD_DATABASE_URL_FILE to be set",
        ));
    };
    craftcloud_db::migrate::verify(&pool)
        .await
        .map_err(|error| Failure::runtime(format!("cannot verify database schema: {error}")))?;
    let references = craftcloud_backup::release_backup_pins(&pool, backup_id, digest, true)
        .await
        .map_err(backup_failure)?;
    let mut event = craftcloud_db::audit::NewEvent::new("backup.archive_references.release")
        .target("backup", backup_id)
        .detail(
            "references",
            craftcloud_db::audit::Detail::Count(i64::try_from(references).unwrap_or(i64::MAX)),
        );
    if let Some(settings) = &config.archive {
        let (provider, host) = settings.audit_endpoint();
        event = event
            .detail("provider", craftcloud_db::audit::Detail::Code(provider))
            .detail(
                "endpoint_host",
                craftcloud_db::audit::Detail::EndpointHost(host),
            );
    }
    craftcloud_db::audit::record(&pool, &craftcloud_db::audit::AuditCtx::system(), &event)
        .await
        .map_err(|_| {
            Failure::runtime("backup references were released but audit recording failed")
        })?;
    pool.close().await;
    println!("released {references} archive reference pin(s) for backup {backup_id}");
    Ok(())
}

struct ArchiveRuntime {
    store: craftcloud_storage::ArchiveStore,
    crypto: craftcloud_storage::ArchiveCrypto,
}

impl ArchiveRuntime {
    fn access(&self) -> craftcloud_backup::ArchiveAccess<'_> {
        craftcloud_backup::ArchiveAccess {
            store: &self.store,
            crypto: &self.crypto,
            verify_sample: 3,
        }
    }
}

async fn archive_runtime(config: &Config) -> Result<Option<ArchiveRuntime>, Failure> {
    #[cfg(feature = "fault-injection")]
    let test_archive_root =
        std::env::var_os("CC_FAULT_ARCHIVE_LOCAL_DIR").map(std::path::PathBuf::from);
    #[cfg(not(feature = "fault-injection"))]
    let test_archive_root: Option<std::path::PathBuf> = None;

    if config.archive.is_none() && test_archive_root.is_none() {
        return Ok(None);
    }
    // Same fail-closed rule as `run`: until the archive is released, no command
    // may connect to a provider or create archive staging data.
    #[cfg(not(feature = "fault-injection"))]
    if config.archive.is_some() {
        return Err(Failure::config(
            "CRAFTCLOUD_ARCHIVE_STORE is not available in this build",
        ));
    }
    let mut server_key = load_two_factor_key(config.secret_key_file.as_ref()).ok_or_else(|| {
        Failure::config(
            "archive operations require a valid CRAFTCLOUD_SECRET_KEY_FILE with a Base64 32-byte key",
        )
    })?;
    let crypto = craftcloud_storage::ArchiveCrypto::from_server_key(&server_key)
        .map_err(|_| Failure::config("CRAFTCLOUD_SECRET_KEY_FILE cannot derive the archive key"))?;
    server_key.zeroize();

    if let Some(root) = test_archive_root {
        if !root.is_absolute() {
            return Err(Failure::config(
                "CC_FAULT_ARCHIVE_LOCAL_DIR must be an absolute test directory",
            ));
        }
        let store = craftcloud_storage::LocalStore::open(&root)
            .await
            .map_err(|_| Failure::runtime("cannot open the test archive directory"))?;
        return Ok(Some(ArchiveRuntime {
            store: craftcloud_storage::ArchiveStore::Local(store),
            crypto,
        }));
    }

    let Some(settings) = &config.archive else {
        return Ok(None);
    };

    let storage_dir = config.storage_dir.as_ref().ok_or_else(|| {
        Failure::config("archive operations require CRAFTCLOUD_STORAGE_DIR for persistent staging")
    })?;
    let canonical_storage = std::fs::canonicalize(storage_dir).map_err(|_| {
        Failure::config("CRAFTCLOUD_STORAGE_DIR is unavailable for archive staging")
    })?;
    let storage_name = canonical_storage
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Failure::config("CRAFTCLOUD_STORAGE_DIR has no valid final component"))?;
    let staging_root = canonical_storage.with_file_name(format!("{storage_name}.archive-staging"));
    match std::fs::create_dir(&staging_root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => {
            return Err(Failure::runtime(
                "cannot create persistent archive staging directory",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(&staging_root)
            .map_err(|_| Failure::runtime("cannot inspect persistent archive staging directory"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Failure::config(
                "persistent archive staging path is not a plain directory",
            ));
        }
        std::fs::set_permissions(&staging_root, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| Failure::runtime("cannot secure persistent archive staging directory"))?;
    }
    let identity = archive_staging_identity(&canonical_storage, settings);
    bind_archive_staging(&staging_root, &identity)?;
    let staging = match craftcloud_storage::LocalStore::open(&staging_root).await {
        Ok(store) => store,
        Err(_) => {
            return Err(Failure::runtime(
                "cannot initialize temporary archive staging",
            ));
        }
    };

    let mut store = match settings {
        ArchiveSettings::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id_file,
            secret_access_key_file,
        } => {
            let access_key = secret::load_secret_file(access_key_id_file).map_err(|_| {
                Failure::config(
                    "CRAFTCLOUD_ARCHIVE_S3_ACCESS_KEY_ID_FILE is unavailable or invalid",
                )
            })?;
            let secret_key = secret::load_secret_file(secret_access_key_file).map_err(|_| {
                Failure::config(
                    "CRAFTCLOUD_ARCHIVE_S3_SECRET_ACCESS_KEY_FILE is unavailable or invalid",
                )
            })?;
            let provider = craftcloud_storage::S3Store::connect(
                craftcloud_storage::S3Config {
                    endpoint: endpoint.clone(),
                    region: region.clone(),
                    bucket: bucket.clone(),
                    prefix: prefix.clone(),
                    access_key_id: Zeroizing::new(access_key.expose().to_owned()),
                    secret_access_key: Zeroizing::new(secret_key.expose().to_owned()),
                },
                staging,
            )
            .map_err(|_| Failure::config("S3 archive provider configuration is invalid"))?;
            craftcloud_storage::ArchiveStore::S3(provider)
        }
        ArchiveSettings::Sftp {
            host,
            port,
            username,
            base_directory,
            private_key_file,
            known_hosts_file,
        } => {
            let private_key = secret::load_secret_file(private_key_file).map_err(|_| {
                Failure::config(
                    "CRAFTCLOUD_ARCHIVE_SFTP_PRIVATE_KEY_FILE is unavailable or invalid",
                )
            })?;
            let provider = craftcloud_storage::SftpStore::connect(
                craftcloud_storage::SftpConfig {
                    host: host.clone(),
                    port: *port,
                    username: username.clone(),
                    private_key_openssh: Zeroizing::new(private_key.expose().to_owned()),
                    known_hosts_file: known_hosts_file.clone(),
                    base_directory: base_directory.clone(),
                    max_connections: 4,
                },
                staging,
            )
            .map_err(|_| Failure::config("SFTP archive provider configuration is invalid"))?;
            craftcloud_storage::ArchiveStore::Sftp(provider)
        }
    };
    store.set_bandwidth_limit_mbps(config.archive_max_mbps);
    Ok(Some(ArchiveRuntime { store, crypto }))
}

fn archive_staging_identity(storage_root: &std::path::Path, settings: &ArchiveSettings) -> String {
    let mut hasher = Sha256::new();
    hasher.update(storage_root.as_os_str().to_string_lossy().as_bytes());
    match settings {
        ArchiveSettings::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            ..
        } => {
            hasher.update(b"s3\0");
            hasher.update(endpoint.as_bytes());
            hasher.update([0]);
            hasher.update(region.as_bytes());
            hasher.update([0]);
            hasher.update(bucket.as_bytes());
            hasher.update([0]);
            hasher.update(prefix.as_deref().unwrap_or_default().as_bytes());
        }
        ArchiveSettings::Sftp {
            host,
            port,
            username,
            base_directory,
            ..
        } => {
            hasher.update(b"sftp\0");
            hasher.update(host.as_bytes());
            hasher.update(port.to_be_bytes());
            hasher.update(username.as_bytes());
            hasher.update([0]);
            hasher.update(base_directory.as_bytes());
        }
    }
    format!(
        "{}\n",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
    )
}

fn bind_archive_staging(root: &std::path::Path, expected: &str) -> Result<(), Failure> {
    let marker = root.join("craftcloud-archive-staging.json");
    match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(Failure::config(
                    "archive staging identity marker is not a plain file",
                ));
            }
            let actual = std::fs::read_to_string(&marker)
                .map_err(|_| Failure::runtime("cannot read archive staging identity marker"))?;
            if actual != expected {
                return Err(Failure::config(
                    "archive staging is bound to a different store or provider",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
                .map_err(|_| Failure::runtime("cannot create archive staging identity marker"))?;
            file.write_all(expected.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|_| Failure::runtime("cannot persist archive staging identity marker"))?;
        }
        Err(_) => {
            return Err(Failure::runtime(
                "cannot inspect archive staging identity marker",
            ));
        }
    }
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
    )
    .with_retention_environment([
        std::env::var_os("CRAFTCLOUD_VERSION_RETENTION_DAYS").is_some(),
        std::env::var_os("CRAFTCLOUD_TRASH_RETENTION_DAYS").is_some(),
        std::env::var_os("CRAFTCLOUD_AUDIT_RETENTION_DAYS").is_some(),
        std::env::var_os("CRAFTCLOUD_QUARANTINE_DAYS").is_some(),
    ]);
    let job = job.with_version_setting_environment([
        std::env::var_os("CRAFTCLOUD_VERSIONS_KEEP_ALL_HOURS").is_some(),
        std::env::var_os("CRAFTCLOUD_VERSIONS_DAILY_DAYS").is_some(),
        std::env::var_os("CRAFTCLOUD_VERSIONS_WEEKLY_WEEKS").is_some(),
        std::env::var_os("CRAFTCLOUD_VERSIONS_MAX_PER_FILE").is_some(),
    ]);
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
    // Fail closed until the archive provider and worker are connected to all
    // download, backup, restore and maintenance paths. Ignoring this setting
    // would falsely report a working archive to the operator.
    if std::env::var_os("CRAFTCLOUD_ARCHIVE_STORE").is_some() || {
        #[cfg(feature = "fault-injection")]
        {
            std::env::var_os("CC_FAULT_ARCHIVE_LOCAL_DIR").is_some()
        }
        #[cfg(not(feature = "fault-injection"))]
        {
            false
        }
    } {
        return Err(Failure::config(
            "CRAFTCLOUD_ARCHIVE_STORE is not available in this build",
        ));
    }
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
    let app_catalog = match &config.app_catalog_file {
        Some(path) => {
            let file = std::fs::File::open(path).map_err(|_| {
                Failure::config("CRAFTCLOUD_APP_CATALOG_FILE is unavailable or unreadable")
            })?;
            let contents = read_app_catalog(file).map_err(|_| {
                Failure::config(
                    "CRAFTCLOUD_APP_CATALOG_FILE is oversized, unreadable, or not UTF-8",
                )
            })?;
            craftcloud_core::apps::extend_app_catalog(
                craftcloud_core::apps::default_app_catalog(),
                &contents,
            )
            .map_err(|_| Failure::config("CRAFTCLOUD_APP_CATALOG_FILE has an invalid catalog"))?
        }
        None => craftcloud_core::apps::default_app_catalog(),
    };
    // Validate the bootstrap secret first: a bad file is a configuration
    // error (exit 2) and must stop startup before anything is bound.
    let bootstrap = load_bootstrap_token(&config)?;
    let metrics_token = load_metrics_token(&config)?;
    let two_factor_key = load_two_factor_key(config.secret_key_file.as_ref());
    let two_factor_old_key = load_two_factor_key(config.secret_key_old_file.as_ref());
    let figcraft = load_figcraft(&config, two_factor_key.is_some())?;
    if two_factor_key.is_none() {
        tracing::warn!("two-factor setup is unavailable: current secret key is not usable");
    }
    if config.mail.is_some() && two_factor_key.is_none() {
        return Err(Failure::config(
            "CRAFTCLOUD_SECRET_KEY_FILE must contain a valid AC-02 key when mail delivery is enabled",
        ));
    }
    let mail_status = craftcloud_mail::TransportStatus::default();
    let metrics = std::sync::Arc::new(Metrics::new());
    // Migrations are never applied implicitly: only verify the schema.
    let pool = connect_database(&config.database).await?;
    let archive_runtime = archive_runtime(&config).await?;
    let archive_parts = archive_runtime.map(|runtime| {
        (
            std::sync::Arc::new(runtime.store),
            std::sync::Arc::new(runtime.crypto),
        )
    });
    if let Some(pool) = &pool {
        craftcloud_db::migrate::verify(pool).await.map_err(|err| {
            Failure::runtime(format!(
                "{err}; run `craftcloud-server migrate` (back up the database first)"
            ))
        })?;
        tracing::info!("database schema verified");
        let has_archive = craftcloud_db::maintenance::has_nonlocal_archive(pool)
            .await
            .map_err(|_| Failure::runtime("cannot verify archive state before startup"))?;
        if has_archive && config.archive.is_none() {
            return Err(Failure::config(
                "database contains archived objects but no archive provider is configured",
            ));
        }
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
    let mail_transport = std::sync::Arc::new(craftcloud_mail::SwitchableTransport::default());
    let email_delivery_enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mail_reload_tx, mut mail_reload_rx) =
        tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<bool>>();
    if let (Some(pool), Some(key)) = (&pool, two_factor_key) {
        match mail_settings::load(
            pool,
            &config,
            Some(&key),
            two_factor_old_key.as_ref(),
            mail_status.clone(),
        )
        .await
        {
            Ok(transport) => {
                if transport.is_none() {
                    mail_status.disable();
                }
                email_delivery_enabled.store(
                    transport
                        .as_ref()
                        .is_some_and(craftcloud_mail::MailTransport::is_enabled),
                    std::sync::atomic::Ordering::Release,
                );
                mail_transport.replace(transport.map(std::sync::Arc::new));
            }
            Err(_) => {
                mail_status.disable();
                mail_status.configuration_error();
                tracing::error!("mail configuration is invalid; delivery disabled");
            }
        }
    } else if config.mail.is_some() {
        mail_status.configuration_error();
        tracing::error!(
            "mail configuration requires a database and a valid secret key; delivery disabled"
        );
    }
    if let Some(pool) = pool.clone() {
        let transport = std::sync::Arc::clone(&mail_transport);
        let enabled = std::sync::Arc::clone(&email_delivery_enabled);
        let config = config.clone();
        let status = mail_status.clone();
        tokio::spawn(async move {
            while let Some(reply) = mail_reload_rx.recv().await {
                let result = match two_factor_key {
                    Some(key) => {
                        mail_settings::load(
                            &pool,
                            &config,
                            Some(&key),
                            two_factor_old_key.as_ref(),
                            status.clone(),
                        )
                        .await
                    }
                    None => Err("mail settings are invalid"),
                };
                let success = match result {
                    Ok(next) => {
                        if next.is_none() {
                            status.disable();
                        }
                        enabled.store(next.is_some(), std::sync::atomic::Ordering::Release);
                        transport.replace(next.map(std::sync::Arc::new));
                        true
                    }
                    Err(_) => {
                        transport.replace(None);
                        enabled.store(false, std::sync::atomic::Ordering::Release);
                        status.disable();
                        status.configuration_error();
                        tracing::error!("mail settings reload failed; delivery disabled");
                        false
                    }
                };
                let _ = reply.send(success);
            }
        });
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
    if let Some(pool) = &pool {
        audit_archive_configuration(pool, &config).await?;
    }
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
        app_catalog,
        idle_timeout: config.session_idle_timeout,
        absolute_timeout: config.session_absolute_timeout,
        device_access_ttl: config.device_access_ttl,
        device_idle_timeout: config.device_idle_timeout,
        device_absolute_timeout: config.device_absolute_timeout,
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
    // Uploads and downloads use the store only while it belongs to this
    // database; on a mismatch the routes answer 503 STORAGE_UNAVAILABLE and the
    // maintenance job leaves the store alone.
    let mut archive_worker_storage_allowed = false;
    let serving_store = match (&pool, &object_store) {
        (Some(pool), Some(store)) => {
            match check_store_binding(pool, store, time::OffsetDateTime::now_utc(), true).await {
                Ok(Binding::Created(id)) => {
                    archive_worker_storage_allowed = true;
                    tracing::info!(store = %id, "object store bound to this database");
                    object_store.clone()
                }
                Ok(Binding::Matches(_)) => {
                    archive_worker_storage_allowed = true;
                    object_store.clone()
                }
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
    let maintenance_task = pool
        .as_ref()
        .filter(|_| config.maintenance_enabled)
        .map(|pool| {
            let job = Maintenance::new(
                pool.clone(),
                serving_store.clone(),
                std::sync::Arc::new(SystemClock),
                maintenance_settings(&config),
            )
            .with_retention_environment([
                std::env::var_os("CRAFTCLOUD_VERSION_RETENTION_DAYS").is_some(),
                std::env::var_os("CRAFTCLOUD_TRASH_RETENTION_DAYS").is_some(),
                std::env::var_os("CRAFTCLOUD_AUDIT_RETENTION_DAYS").is_some(),
                std::env::var_os("CRAFTCLOUD_QUARANTINE_DAYS").is_some(),
            ])
            .with_version_setting_environment([
                std::env::var_os("CRAFTCLOUD_VERSIONS_KEEP_ALL_HOURS").is_some(),
                std::env::var_os("CRAFTCLOUD_VERSIONS_DAILY_DAYS").is_some(),
                std::env::var_os("CRAFTCLOUD_VERSIONS_WEEKLY_WEEKS").is_some(),
                std::env::var_os("CRAFTCLOUD_VERSIONS_MAX_PER_FILE").is_some(),
            ])
            .with_metrics(std::sync::Arc::clone(&metrics));
            let job = match (&archive_parts, archive_worker_storage_allowed) {
                (Some((store, _)), true) => job.with_archive(std::sync::Arc::clone(store)),
                _ => job,
            };
            tracing::info!(
                interval_secs = config.maintenance_interval.as_secs(),
                "maintenance job scheduled"
            );
            tokio::spawn(std::sync::Arc::new(job).run_loop(config.maintenance_interval, stop_rx))
        });
    let services = pool.as_ref().map(|pool| {
        let (setting_values, setting_locked) = instance_setting_environment(&config);
        let storage_path = config.storage_dir.clone();
        let services = Services::new(pool.clone(), bootstrap, auth_settings)
            .with_maintenance_enabled(config.maintenance_enabled)
            .with_storage_free_bytes(std::sync::Arc::new(move || {
                storage_path
                    .as_deref()
                    .and_then(|path| fs2::available_space(path).ok())
            }))
            .with_instance_setting_environment(setting_values, setting_locked)
            .with_two_factor_keys(two_factor_key, two_factor_old_key)
            .with_email_delivery_state(std::sync::Arc::clone(&email_delivery_enabled))
            .with_instance_settings_changed(std::sync::Arc::new({
                let reload = mail_reload_tx.clone();
                move || {
                    let reload = reload.clone();
                    Box::pin(async move {
                        let (reply, result) = tokio::sync::oneshot::channel();
                        if reload.send(reply).is_err() {
                            return false;
                        }
                        result.await.unwrap_or(false)
                    })
                }
            }))
            .with_email_transport_status(std::sync::Arc::new({
                let status = mail_status.clone();
                move || {
                    let snapshot = status.snapshot();
                    craftcloud_api::EmailTransportStatus {
                        transport: (!snapshot.active_transport.is_empty())
                            .then_some(snapshot.active_transport),
                        sender: (!snapshot.sender.is_empty()).then_some(snapshot.sender),
                        certificate_expires_at: snapshot
                            .certificate_expires_unix
                            .and_then(|unix| time::OffsetDateTime::from_unix_timestamp(unix).ok())
                            .and_then(|date| {
                                date.format(&time::format_description::well_known::Rfc3339)
                                    .ok()
                            }),
                        last_error_code: snapshot.last_safe_error,
                    }
                }
            }))
            .with_metrics(std::sync::Arc::clone(&metrics));
        let services = match serving_store.clone() {
            Some(store) => services.with_storage(store),
            None => services,
        };
        let services = match (
            &archive_parts,
            &config.archive,
            archive_worker_storage_allowed,
        ) {
            (Some((store, crypto)), Some(settings), true) => {
                let (provider_type, endpoint) = match settings {
                    ArchiveSettings::S3 { endpoint, .. } => ("s3", Some(endpoint.clone())),
                    ArchiveSettings::Sftp { host, port, .. } => {
                        let authority = if host.contains(':') {
                            format!("[{host}]:{port}")
                        } else {
                            format!("{host}:{port}")
                        };
                        ("sftp", Some(authority))
                    }
                };
                services
                    .with_archive(std::sync::Arc::clone(store), std::sync::Arc::clone(crypto))
                    .with_archive_after(config.archive_after)
                    .with_archive_operator_metadata(provider_type.to_owned(), endpoint)
            }
            _ => services,
        };
        let services = match figcraft {
            Some(loaded) => services
                .with_figcraft(loaded.client, loaded.public_url)
                .with_figcraft_internal_url(loaded.internal_url),
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
    let figcraft_task = services.as_ref().map(|services| {
        tokio::spawn(craftcloud_api::run_figcraft_worker(
            std::sync::Arc::clone(services),
            stop_maintenance.subscribe(),
        ))
    });
    let mail_task = match (&pool, two_factor_key) {
        (Some(pool), Some(key)) => {
            let public_url = config.public_url.as_deref().unwrap_or_default();
            let domain = mail_message_id_domain(public_url);
            tracing::info!("mail delivery worker enabled");
            let public_url = config.public_url.clone().unwrap_or_default();
            Some(tokio::spawn(mail_worker::run(
                pool.clone(),
                std::sync::Arc::clone(&mail_transport),
                key,
                two_factor_old_key,
                public_url,
                domain,
                stop_maintenance.subscribe(),
            )))
        }
        _ => None,
    };
    let archive_worker_task = match (&pool, &serving_store, &archive_parts) {
        (Some(pool), Some(local), Some((archive_store, crypto)))
            if config.maintenance_enabled && archive_worker_storage_allowed =>
        {
            let pool = pool.clone();
            let local = local.clone();
            let archive_store = std::sync::Arc::clone(archive_store);
            let crypto = std::sync::Arc::clone(crypto);
            let mut stop = stop_maintenance.subscribe();
            let interval = config
                .maintenance_interval
                .max(std::time::Duration::from_secs(1));
            let archive_after = config.archive_after;
            let verify_budget = craftcloud_api::archive_worker::ArchiveVerifyBudget {
                max_bytes: config.archive_verify_bytes_per_run,
                max_time: config.archive_verify_time_per_run,
            };
            tracing::info!(
                interval_secs = interval.as_secs(),
                archive_after_secs = archive_after.as_secs(),
                "archive worker scheduled"
            );
            Some(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        changed = stop.changed() => {
                            if changed.is_err() || *stop.borrow() {
                                break;
                            }
                        }
                        () = tokio::time::sleep(interval) => {
                            match craftcloud_api::archive_worker::archive_batch_with_verify_budget(
                                &pool,
                                &local,
                                archive_store.as_ref(),
                                crypto.as_ref(),
                                time::OffsetDateTime::now_utc(),
                                archive_after,
                                verify_budget,
                            ).await {
                                Ok(batch) if batch.attempted > 0 || batch.archived > 0 || batch.retried > 0 => {
                                    tracing::info!(attempted = batch.attempted, archived = batch.archived, retried = batch.retried, "archive batch completed");
                                }
                                Ok(_) => {}
                                Err(_) => tracing::error!("archive batch failed; durable claims remain recoverable"),
                            }
                        }
                    }
                }
            }))
        }
        _ => None,
    };
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
    if let Some(services) = &services
        && tokio::time::timeout(config.shutdown_grace, services.drain_figcraft_jobs())
            .await
            .is_err()
    {
        tracing::warn!("FigCraft HTTP operations did not drain before the shutdown deadline");
    }
    // Stop the maintenance loop between items; a run in progress finishes its item.
    let _ = stop_maintenance.send(true);
    if let Some(mut task) = figcraft_task
        && tokio::time::timeout(config.shutdown_grace, &mut task)
            .await
            .is_err()
    {
        task.abort();
        let _ = task.await;
        tracing::warn!("FigCraft worker stopped at shutdown deadline; durable work remains queued");
    }
    if let Some(mut task) = mail_task
        && tokio::time::timeout(config.shutdown_grace, &mut task)
            .await
            .is_err()
    {
        task.abort();
        let _ = task.await;
        tracing::warn!("mail worker stopped at shutdown deadline; durable mail remains queued");
    }
    if let Some(mut task) = archive_worker_task
        && tokio::time::timeout(config.shutdown_grace, &mut task)
            .await
            .is_err()
    {
        task.abort();
        let _ = task.await;
        tracing::warn!(
            "archive worker stopped at shutdown deadline; durable claims remain recoverable"
        );
    }
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
    if let Some(pool) = pool
        && tokio::time::timeout(config.shutdown_grace, pool.close())
            .await
            .is_err()
    {
        tracing::warn!("database pool did not close before the shutdown deadline");
    }
    Ok(())
}

fn mail_message_id_domain(public_url: &str) -> String {
    match url::Url::parse(public_url)
        .ok()
        .and_then(|url| url.host().map(|host| host.to_owned()))
    {
        Some(url::Host::Ipv4(address)) => format!("[{address}]"),
        Some(url::Host::Ipv6(address)) => format!("[{address}]"),
        Some(url::Host::Domain(domain)) => domain,
        None => "craftcloud.invalid".to_owned(),
    }
}

fn instance_setting_environment(
    config: &Config,
) -> (BTreeMap<String, serde_json::Value>, BTreeSet<String>) {
    instance_setting_environment_with(config, &|name| {
        std::env::var_os(format!("CRAFTCLOUD_{name}")).is_some()
    })
}

fn instance_setting_environment_with(
    config: &Config,
    env_set: &dyn Fn(&str) -> bool,
) -> (BTreeMap<String, serde_json::Value>, BTreeSet<String>) {
    let mut values = BTreeMap::new();
    let mut locked = BTreeSet::new();
    let mut public = |key: &str, envs: &[&str], value: serde_json::Value| {
        if envs.iter().any(|name| env_set(name)) {
            locked.insert(key.to_owned());
            values.insert(key.to_owned(), value);
        }
    };
    public(
        "storage.compression_level",
        &["STORAGE_COMPRESSION_LEVEL"],
        json!(config.storage_compression_level),
    );
    let transport = config
        .mail
        .as_ref()
        .map(|mail| match mail {
            config::MailSettings::Smtp { .. } => "smtp",
            config::MailSettings::Graph { .. } => "graph",
            config::MailSettings::SmtpOAuthMicrosoft { .. } => "smtp_oauth_microsoft",
            config::MailSettings::SmtpOAuthGoogle { .. } => "smtp_oauth_google",
        })
        .unwrap_or("none");
    public("mail.transport", &["MAIL_TRANSPORT"], json!(transport));
    if let Some(mail) = &config.mail {
        let (host, port, tls, username, from) = match mail {
            config::MailSettings::Smtp {
                host,
                port,
                tls,
                username,
                from,
                ..
            } => (
                Some(host),
                Some(port),
                Some(tls),
                username.as_ref(),
                Some(from),
            ),
            config::MailSettings::SmtpOAuthGoogle {
                host,
                port,
                tls,
                username,
                from,
                ..
            } => (
                Some(host),
                Some(port),
                Some(tls),
                Some(username),
                Some(from),
            ),
            config::MailSettings::SmtpOAuthMicrosoft {
                host,
                port,
                tls,
                username,
                from,
                ..
            } => (
                Some(host),
                Some(port),
                Some(tls),
                Some(username),
                Some(from),
            ),
            config::MailSettings::Graph { .. } => (None, None, None, None, None),
        };
        public("mail.host", &["SMTP_HOST"], json!(host));
        public("mail.port", &["SMTP_PORT"], json!(port));
        public(
            "mail.tls",
            &["SMTP_TLS"],
            json!(tls.map(|v| match v {
                craftcloud_mail::SmtpTls::StartTls => "starttls",
                craftcloud_mail::SmtpTls::ImplicitTls => "implicit",
            })),
        );
        public("mail.username", &["SMTP_USERNAME"], json!(username));
        public("mail.from", &["SMTP_FROM"], json!(from));
        match mail {
            config::MailSettings::Graph {
                tenant_id,
                client_id,
                sender,
                save_to_sent,
                ..
            } => {
                public("mail.tenant_id", &["GRAPH_TENANT_ID"], json!(tenant_id));
                public("mail.client_id", &["GRAPH_CLIENT_ID"], json!(client_id));
                public("mail.sender", &["GRAPH_SENDER"], json!(sender));
                public(
                    "mail.save_to_sent",
                    &["GRAPH_SAVE_TO_SENT"],
                    json!(save_to_sent),
                );
            }
            config::MailSettings::SmtpOAuthMicrosoft {
                tenant_id,
                client_id,
                ..
            } => {
                public(
                    "mail.tenant_id",
                    &["SMTP_OAUTH_TENANT_ID"],
                    json!(tenant_id),
                );
                public(
                    "mail.client_id",
                    &["SMTP_OAUTH_CLIENT_ID"],
                    json!(client_id),
                );
            }
            _ => {}
        }
    }
    public(
        "retention.version_days",
        &["VERSION_RETENTION_DAYS"],
        json!(config.version_retention.as_secs() / 86_400),
    );
    public(
        "retention.trash_days",
        &["TRASH_RETENTION_DAYS"],
        json!(config.trash_retention.as_secs() / 86_400),
    );
    public(
        "retention.audit_days",
        &["AUDIT_RETENTION_DAYS"],
        json!(config.audit_retention_days),
    );
    public(
        "retention.quarantine_days",
        &["QUARANTINE_DAYS"],
        json!(config.quarantine_retention.as_secs() / 86_400),
    );
    public(
        "versions.keep_all_hours",
        &["VERSIONS_KEEP_ALL_HOURS"],
        json!(config.version_keep_all_hours),
    );
    public(
        "versions.daily_days",
        &["VERSIONS_DAILY_DAYS"],
        json!(config.version_daily_days),
    );
    public(
        "versions.weekly_weeks",
        &["VERSIONS_WEEKLY_WEEKS"],
        json!(config.version_weekly_weeks),
    );
    public(
        "versions.max_per_file",
        &["VERSIONS_MAX_PER_FILE"],
        json!(config.version_max_per_file),
    );
    drop(public);
    let secret_envs: [(&str, &[&str]); 5] = [
        ("mail.password", &["SMTP_PASSWORD_FILE"]),
        (
            "mail.client_secret",
            &["SMTP_OAUTH_CLIENT_SECRET_FILE", "GRAPH_CLIENT_SECRET_FILE"],
        ),
        (
            "mail.certificate",
            &["SMTP_OAUTH_CERT_FILE", "GRAPH_CERT_FILE"],
        ),
        (
            "mail.private_key",
            &["SMTP_OAUTH_KEY_FILE", "GRAPH_KEY_FILE"],
        ),
        ("mail.service_account", &["SMTP_OAUTH_SERVICE_ACCOUNT_FILE"]),
    ];
    for (key, envs) in secret_envs {
        if envs.iter().any(|name| env_set(name)) {
            locked.insert(key.to_owned());
        }
    }
    // Environment mail configuration is authoritative as a whole: every mail
    // setting and credential is locked, so a stored database value can never
    // be mixed into (or replace part of) the environment-defined transport.
    if config.mail.is_some() || env_set("MAIL_TRANSPORT") {
        locked.extend(mail_setting_keys().map(str::to_owned));
    }
    (values, locked)
}

/// Every mail setting and credential key that environment mail locks.
fn mail_setting_keys() -> impl Iterator<Item = &'static str> {
    [
        "mail.transport",
        "mail.host",
        "mail.port",
        "mail.tls",
        "mail.username",
        "mail.from",
        "mail.tenant_id",
        "mail.client_id",
        "mail.sender",
        "mail.save_to_sent",
        "mail.password",
        "mail.client_secret",
        "mail.certificate",
        "mail.private_key",
        "mail.service_account",
    ]
    .into_iter()
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
    use craftcloud_storage::{ObjectStore, WriteSink};

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn roles_select_their_commands() {
        assert!(matches!(parse_args(&words(&["serve"])), Ok(Command::Serve)));
        assert!(matches!(parse_args(&[]), Ok(Command::Serve)));
        assert!(matches!(
            parse_args(&words(&["figcraft"])),
            Ok(Command::Figcraft)
        ));
        assert!(matches!(
            parse_args(&words(&["all-in-one"])),
            Ok(Command::AllInOne)
        ));
        assert!(matches!(
            parse_args(&words(&["migrate"])),
            Ok(Command::Migrate)
        ));
        assert!(matches!(
            parse_args(&words(&["healthcheck"])),
            Ok(Command::Healthcheck)
        ));
        assert!(matches!(
            parse_args(&words(&["backup", "create", "--out", "/backups/a"])),
            Ok(Command::BackupCreate { out }) if out == "/backups/a"
        ));
        assert!(matches!(
            parse_args(&words(&["restore", "/backups/a"])),
            Ok(Command::BackupRestore { dir, digest: None }) if dir == "/backups/a"
        ));
    }

    #[test]
    fn compression_environment_setting_is_exposed_as_locked_operator_setting() {
        let config = Config::from_env_map(&BTreeMap::from([(
            "CRAFTCLOUD_STORAGE_COMPRESSION_LEVEL".to_owned(),
            "0".to_owned(),
        )]))
        .unwrap();
        let (values, locked) =
            instance_setting_environment_with(&config, &|name| name == "STORAGE_COMPRESSION_LEVEL");
        assert!(locked.contains("storage.compression_level"));
        assert_eq!(values["storage.compression_level"], serde_json::json!(0));
    }

    #[test]
    fn an_unknown_role_is_refused_and_the_roles_are_listed() {
        let Err(message) = parse_args(&words(&["frobnicate"])) else {
            panic!("an unknown role must be refused");
        };
        assert!(message.contains("frobnicate"), "{message}");
        assert!(message.contains("all-in-one"), "{message}");
        assert!(parse_args(&words(&["serve", "extra"])).is_err());
        assert!(parse_args(&words(&["backup"])).is_err());
    }

    #[test]
    fn app_catalog_read_is_bounded_and_requires_utf8() {
        let exact = vec![b'a'; MAX_APP_CATALOG_BYTES as usize];
        assert_eq!(
            read_app_catalog(std::io::Cursor::new(exact)).unwrap().len(),
            MAX_APP_CATALOG_BYTES as usize
        );
        let oversized = vec![b'a'; MAX_APP_CATALOG_BYTES as usize + 1];
        assert_eq!(
            read_app_catalog(std::io::Cursor::new(oversized))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            read_app_catalog(std::io::Cursor::new(b"short catalog".to_vec())).unwrap(),
            "short catalog"
        );
        assert_eq!(
            read_app_catalog(std::io::Cursor::new(vec![0xff]))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn recall_retry_removes_only_its_orphaned_temp_and_publishes() {
        let root = std::env::temp_dir().join(format!(
            "craftcloud-server-recall-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = craftcloud_storage::LocalStore::open(&root).await.unwrap();
        let key = craftcloud_storage::ObjectKey::generate();
        std::fs::write(
            root.join("tmp").join(format!("{}.part", key.as_str())),
            b"partial data from interrupted recall",
        )
        .unwrap();

        remove_stale_recall_temp(&store, &key).await.unwrap();
        let plaintext = b"verified plaintext after retry";
        let expected: craftcloud_storage::Sha256Digest = Sha256::digest(plaintext).into();
        let mut temp = store
            .begin_write_as(key.clone(), plaintext.len() as u64)
            .await
            .unwrap();
        temp.write(plaintext).await.unwrap();
        store
            .finalize_as(temp, key.clone(), plaintext.len() as u64, &expected)
            .await
            .unwrap();
        store.verify(&key, &expected).await.unwrap();

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn argument_parsing() {
        assert!(matches!(parse_args(&args(&[])), Ok(Command::Serve)));
        assert!(matches!(
            parse_args(&args(&["migrate"])),
            Ok(Command::Migrate)
        ));
        assert!(matches!(
            parse_args(&args(&["healthcheck"])),
            Ok(Command::Healthcheck)
        ));
        assert!(matches!(
            parse_args(&args(&["archive", "status"])),
            Ok(Command::ArchiveStatus)
        ));
        assert!(matches!(
            parse_args(&args(&["archive", "recall"])),
            Ok(Command::ArchiveRecall { org_id: None })
        ));
        let org_id = uuid::Uuid::new_v4();
        assert!(matches!(
            parse_args(&args(&["archive", "recall", "--org", &org_id.to_string()])),
            Ok(Command::ArchiveRecall { org_id: Some(id) }) if id == org_id
        ));
        assert!(parse_args(&args(&["archive", "recall", "--org", "bad"])).is_err());
        assert!(parse_args(&args(&["healthcheck", "extra"])).is_err());
        assert!(matches!(parse_args(&args(&["--help"])), Ok(Command::Help)));
        assert!(
            matches!(parse_args(&args(&["admin", "user", "reset-2fa", "user@example.test"])), Ok(Command::ResetTwoFactor { email }) if email == "user@example.test")
        );
        assert!(matches!(
            parse_args(&args(&["--version"])),
            Ok(Command::Version)
        ));
        assert!(matches!(parse_args(&args(&["-V"])), Ok(Command::Version)));
        assert!(parse_args(&args(&["nope"])).is_err());
        assert!(parse_args(&args(&["migrate", "extra"])).is_err());
    }

    #[test]
    fn backup_arguments() {
        assert!(matches!(
            parse_args(&args(&["admin", "backup", "create", "--out", "/b/1"])),
            Ok(Command::BackupCreate { out }) if out == "/b/1"
        ));
        assert!(matches!(
            parse_args(&args(&["admin", "backup", "verify", "/b/1"])),
            Ok(Command::BackupVerify { dir, digest: None }) if dir == "/b/1"
        ));
        assert!(matches!(
            parse_args(&args(&[
                "admin",
                "backup",
                "restore",
                "/b/1",
                "--manifest-sha256",
                "ab"
            ])),
            Ok(Command::BackupRestore { dir, digest: Some(d) }) if dir == "/b/1" && d == "ab"
        ));
        assert!(matches!(
            parse_args(&args(&[
                "admin", "backup", "inventory", "--confirm-complete", "/b/1", "/b/2"
            ])),
            Ok(Command::BackupInventory { dirs }) if dirs == ["/b/1", "/b/2"]
        ));
        assert!(matches!(
            parse_args(&args(&["admin", "backup", "inventory", "--confirm-complete"])),
            Ok(Command::BackupInventory { dirs }) if dirs.is_empty()
        ));
        assert!(matches!(
            parse_args(&args(&[
                "admin", "backup", "release", "--id", "d7d6c52f-cb40-42ca-a6a8-7bf4e73db845",
                "--manifest-sha256", &"a".repeat(64), "--confirm-deleted"
            ])),
            Ok(Command::BackupRelease { backup_id, digest })
                if backup_id == "d7d6c52f-cb40-42ca-a6a8-7bf4e73db845" && digest == "a".repeat(64)
        ));
        for bad in [
            &["admin", "backup"][..],
            &["admin", "backup", "create"][..],
            &["admin", "backup", "create", "/b/1"][..],
            &["admin", "backup", "verify"][..],
            &["admin", "backup", "inventory", "/b/1"][..],
            &["admin", "backup", "release", "--id", "x"][..],
            &["admin", "backup", "restore", "/b/1", "--other", "x"][..],
            &["admin", "backup", "wipe", "/b/1"][..],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn storage_rechunk_requires_an_explicit_target_and_accepts_dry_run() {
        let file_id = uuid::Uuid::new_v4();
        assert!(matches!(
            parse_args(&args(&["admin", "storage", "rechunk", "--file", &file_id.to_string(), "--dry-run"])),
            Ok(Command::StorageRechunk { file_id: Some(id), dry_run: true }) if id == file_id
        ));
        assert!(matches!(
            parse_args(&args(&["admin", "storage", "rechunk", "--file", &file_id.to_string()])),
            Ok(Command::StorageRechunk { file_id: Some(id), dry_run: false }) if id == file_id
        ));
        assert!(matches!(
            parse_args(&args(&[
                "admin",
                "storage",
                "rechunk",
                "--all",
                "--dry-run"
            ])),
            Ok(Command::StorageRechunk {
                file_id: None,
                dry_run: true
            })
        ));
        assert!(matches!(
            parse_args(&args(&["admin", "storage", "rechunk", "--all"])),
            Ok(Command::StorageRechunk {
                file_id: None,
                dry_run: false
            })
        ));
        for bad in [
            vec!["admin", "storage", "rechunk", "--file", "bad", "--dry-run"],
            vec!["admin", "storage", "rechunk", "--all", "--force"],
        ] {
            assert!(parse_args(&args(&bad)).is_err(), "{bad:?}");
        }
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
