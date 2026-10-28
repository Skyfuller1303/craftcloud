//! CraftCloud server binary: reads configuration, composes the router and
//! serves it until a shutdown signal arrives.
//!
//! Exit codes: 0 clean shutdown, 1 runtime failure, 2 invalid configuration.

mod config;
mod secret;
mod server;

use std::process::ExitCode;

use config::{Config, LogFormat};
use craftcloud_api::ApiConfig;
use craftcloud_core::Capabilities;
use tracing_subscriber::EnvFilter;

const EXIT_CONFIG: u8 = 2;

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
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

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!("{message}");
            eprintln!("craftcloud-server: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(config: Config) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|err| format!("cannot bind {}: {err}", config.bind))?;
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
    let outcome = server::serve(listener, router, shutdown_signal(), config.shutdown_grace).await?;
    match outcome {
        server::Shutdown::Graceful => tracing::info!("shut down cleanly"),
        server::Shutdown::Forced => {
            tracing::warn!("shut down after grace period; some connections were cut")
        }
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
