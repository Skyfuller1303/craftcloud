//! CraftCloud server binary: reads configuration, composes the router and
//! serves it until a shutdown signal arrives.

use std::net::SocketAddr;
use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

const BIND_ENV: &str = "CRAFTCLOUD_BIND";
const DEFAULT_BIND: &str = "127.0.0.1:8080";

/// Reads the bind address from `CRAFTCLOUD_BIND` (default `127.0.0.1:8080`).
fn bind_address() -> Result<SocketAddr, String> {
    let raw = match std::env::var(BIND_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => DEFAULT_BIND.to_owned(),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!("{BIND_ENV} is not valid unicode"));
        }
    };
    parse_bind(&raw)
}

fn parse_bind(raw: &str) -> Result<SocketAddr, String> {
    raw.parse().map_err(|err| {
        format!(
            "invalid {BIND_ENV} value {raw:?}: {err}; expected <ip>:<port>, e.g. {DEFAULT_BIND}"
        )
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!("{message}");
            eprintln!("craftcloud-server: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let address = bind_address()?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|err| format!("cannot bind {address}: {err}"))?;
    tracing::warn!(
        %address,
        "starting CraftCloud development prototype (not production-ready; no authentication)"
    );
    axum::serve(listener, craftcloud_api::router())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| format!("server error: {err}"))?;
    tracing::info!("shut down cleanly");
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

    #[test]
    fn default_bind_is_loopback() {
        let addr = parse_bind(DEFAULT_BIND).unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 8080);
    }

    #[test]
    fn invalid_bind_values_are_errors() {
        for bad in ["garbage", "", "127.0.0.1", "127.0.0.1:99999", ":8080"] {
            let err = parse_bind(bad).unwrap_err();
            assert!(err.contains(BIND_ENV), "{err}");
        }
    }
}
