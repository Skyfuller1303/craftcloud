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
use craftcloud_api::ApiConfig;
use craftcloud_core::Capabilities;
use craftcloud_db::PgPool;
use tracing_subscriber::EnvFilter;

const EXIT_CONFIG: u8 = 2;

const USAGE: &str = "usage: craftcloud-server [migrate]\n\
    \n\
    (no argument)  start the HTTP server\n\
    migrate        apply pending database migrations and exit\n";

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
    Help,
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::Serve),
        [one] if one == "migrate" => Ok(Command::Migrate),
        [one] if one == "--help" || one == "-h" => Ok(Command::Help),
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
/// only in the zeroizing `Secret` and never logged.
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

async fn run(config: Config) -> Result<(), Failure> {
    // Migrations are never applied implicitly: only verify the schema.
    let pool = connect_database(&config.database).await?;
    if let Some(pool) = &pool {
        craftcloud_db::migrate::verify(pool).await.map_err(|err| {
            Failure::runtime(format!(
                "{err}; run `craftcloud-server migrate` (back up the database first)"
            ))
        })?;
        tracing::info!("database schema verified");
    } else {
        tracing::warn!(
            "no database configured (CRAFTCLOUD_DATABASE_URL_FILE unset); running in prototype mode"
        );
    }

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|err| Failure::runtime(format!("cannot bind {}: {err}", config.bind)))?;
    tracing::warn!(
        address = %config.bind,
        "starting CraftCloud development prototype (not production-ready; no authentication)"
    );
    let router = craftcloud_api::router_with(
        Capabilities::development_defaults(),
        &ApiConfig {
            max_body_bytes: config.max_body_bytes,
            request_timeout: config.request_timeout,
            trusted_proxies: config.trusted_proxies.clone(),
        },
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
