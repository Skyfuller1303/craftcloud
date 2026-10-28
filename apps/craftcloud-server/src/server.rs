//! Serving and shutdown lifecycle.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// How a server run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// All in-flight requests finished within the grace period.
    Graceful,
    /// The grace period elapsed; remaining connections were abandoned.
    Forced,
}

/// Serves `router` on `listener` until `shutdown` completes, then drains
/// in-flight requests for at most `grace`.
pub async fn serve(
    listener: TcpListener,
    router: axum::Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    grace: Duration,
) -> Result<Shutdown, String> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let serving = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        // Receiver gone means the server already stopped; nothing to notify.
        let _ = started_tx.send(());
    })
    .into_future();
    tokio::pin!(serving);

    let grace_elapsed = async {
        // If the sender was dropped without a signal the server has stopped
        // on its own and the other select branch wins.
        if started_rx.await.is_ok() {
            tokio::time::sleep(grace).await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    tokio::select! {
        result = &mut serving => {
            result.map_err(|err| format!("server error: {err}"))?;
            Ok(Shutdown::Graceful)
        }
        () = grace_elapsed => {
            tracing::warn!(
                grace_secs = grace.as_secs(),
                "grace period elapsed; abandoning remaining connections"
            );
            Ok(Shutdown::Forced)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    async fn start(
        grace: Duration,
    ) -> (
        SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<Shutdown, String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(serve(
            listener,
            craftcloud_api::router(),
            async move {
                let _ = rx.await;
            },
            grace,
        ));
        (addr, tx, handle)
    }

    async fn get(addr: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn serves_and_shuts_down_gracefully() {
        let (addr, shutdown, handle) = start(Duration::from_secs(5)).await;
        let live = get(addr, "/health/live").await;
        assert!(live.starts_with("HTTP/1.1 200"), "{live}");
        assert!(live.contains(r#"{"status":"live"}"#), "{live}");
        let info = get(addr, "/api/v1/info").await;
        assert!(info.contains("development-prototype"), "{info}");

        shutdown.send(()).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server did not stop within the bound")
            .unwrap();
        assert_eq!(outcome, Ok(Shutdown::Graceful));
        assert!(TcpStream::connect(addr).await.is_err());
    }

    #[tokio::test]
    async fn idle_keep_alive_connection_is_bounded_by_grace() {
        let (addr, shutdown, handle) = start(Duration::from_secs(1)).await;
        // An open connection that sends nothing keeps graceful shutdown waiting.
        let _idle = TcpStream::connect(addr).await.unwrap();
        shutdown.send(()).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("shutdown exceeded the grace bound")
            .unwrap();
        assert!(outcome.is_ok());
    }
}
