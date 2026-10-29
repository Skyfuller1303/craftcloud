//! `craftcloud-server healthcheck`: a container healthcheck that needs no
//! shell, `curl` or `wget` in the image.
//!
//! It sends `GET /health/ready` to the server's own listener
//! (`CRAFTCLOUD_BIND`, default `127.0.0.1:8080`; an unspecified address such
//! as `0.0.0.0` is reached through loopback) and succeeds only on a `200`
//! answer. It never uses the public address and never reads secrets.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

/// Same default as `Config`.
const DEFAULT_BIND: &str = "127.0.0.1:8080";
const TIMEOUT: Duration = Duration::from_secs(3);
/// Only the status line is needed.
const MAX_RESPONSE_BYTES: u64 = 256;

/// The loopback address of the listener named by `CRAFTCLOUD_BIND`.
pub fn target(bind: Option<&str>) -> Result<SocketAddr, String> {
    let raw = bind.filter(|value| !value.trim().is_empty());
    let addr: SocketAddr = raw
        .unwrap_or(DEFAULT_BIND)
        .trim()
        .parse()
        .map_err(|_| "CRAFTCLOUD_BIND is not a valid <ip>:<port>".to_owned())?;
    let ip = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    Ok(SocketAddr::new(ip, addr.port()))
}

/// True when the first line of `response` is an HTTP status line with `200`.
pub fn is_ok_status(response: &[u8]) -> bool {
    let line = response.split(|byte| *byte == b'\n').next().unwrap_or(&[]);
    let mut parts = line.split(|byte| *byte == b' ').filter(|p| !p.is_empty());
    matches!(
        (parts.next(), parts.next()),
        (Some(version), Some(b"200")) if version.starts_with(b"HTTP/1.")
    )
}

/// Performs the request. `Ok(())` only for a `200` answer.
pub fn run(bind: Option<&str>) -> Result<(), String> {
    let addr = target(bind)?;
    let mut stream =
        TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|err| format!("connect: {err}"))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|err| format!("socket: {err}"))?;
    stream
        .write_all(b"GET /health/ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|err| format!("request: {err}"))?;
    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .map_err(|err| format!("response: {err}"))?;
    if is_ok_status(&response) {
        Ok(())
    } else {
        let line = response.split(|byte| *byte == b'\r').next().unwrap_or(&[]);
        Err(format!(
            "not ready: {}",
            String::from_utf8_lossy(line).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn target_defaults_to_the_server_default_and_maps_unspecified_to_loopback() {
        assert_eq!(target(None).unwrap().to_string(), "127.0.0.1:8080");
        assert_eq!(target(Some("  ")).unwrap().to_string(), "127.0.0.1:8080");
        assert_eq!(
            target(Some("0.0.0.0:9000")).unwrap().to_string(),
            "127.0.0.1:9000"
        );
        assert_eq!(target(Some("[::]:9000")).unwrap().to_string(), "[::1]:9000");
        assert_eq!(
            target(Some("10.1.2.3:81")).unwrap().to_string(),
            "10.1.2.3:81"
        );
        assert!(target(Some("localhost:80")).is_err());
        assert!(target(Some("nonsense")).is_err());
    }

    #[test]
    fn only_a_200_status_line_counts_as_ready() {
        assert!(is_ok_status(
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n"
        ));
        assert!(is_ok_status(b"HTTP/1.0 200 OK\r\n"));
        assert!(!is_ok_status(b"HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(!is_ok_status(b"HTTP/1.1 2000 Odd\r\n"));
        assert!(!is_ok_status(b"HTTP/1.1 301 Moved\r\nx: 200\r\n"));
        assert!(!is_ok_status(b"garbage 200\r\n"));
        assert!(!is_ok_status(b""));
    }

    fn serve_once(reply: &'static [u8]) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0u8; 512];
            let n = stream.read(&mut request).unwrap();
            request.truncate(n);
            stream.write_all(reply).unwrap();
            request
        });
        (bind, handle)
    }

    #[test]
    fn run_requests_the_ready_route_and_accepts_200() {
        let (bind, handle) = serve_once(b"HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ready\"}");
        assert_eq!(run(Some(&bind)), Ok(()));
        let request = handle.join().unwrap();
        assert!(request.starts_with(b"GET /health/ready HTTP/1.1\r\n"));
    }

    #[test]
    fn run_fails_on_503_and_on_a_closed_port() {
        let (bind, handle) = serve_once(b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
        let err = run(Some(&bind)).unwrap_err();
        assert!(err.contains("503"), "{err}");
        handle.join().unwrap();

        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().to_string()
        };
        assert!(run(Some(&closed)).unwrap_err().starts_with("connect:"));
    }
}
