use std::{
    io::{Read, Write},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream},
    process::Command,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use craftcloud_client::{
    CloudClient, CloudError, CloudTransport, OidcDesktopRequest, OidcStartRequest,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const MAX_CALLBACK_BYTES: usize = 8192;
const MAX_INVALID_CALLBACKS: usize = 32;
const MAX_CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Opens a validated authorization URL in the operating system browser.
pub trait Browser: Send + Sync {
    fn open(&self, authorization_url: &str) -> Result<(), CloudError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemBrowser;

impl Browser for SystemBrowser {
    fn open(&self, authorization_url: &str) -> Result<(), CloudError> {
        validate_authorization_url(authorization_url)?;
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = Command::new("rundll32.exe");
            command.args(["url.dll,FileProtocolHandler", authorization_url]);
            command
        };
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut command = Command::new("open");
            command.arg(authorization_url);
            command
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = {
            let mut command = Command::new("xdg-open");
            command.arg(authorization_url);
            command
        };
        command
            .spawn()
            .map(|_| ())
            .map_err(|_| CloudError::Io("could not open system browser".into()))
    }
}

#[derive(Debug, Clone)]
pub struct OidcClientDetails {
    pub organization_id: Uuid,
    pub client_kind: String,
    pub client_id: String,
    pub device_name: String,
}

/// Runs one native OIDC authorization. The listener is bound before the server request and
/// browser launch. Neither request is retried because authorization state is one-use. The
/// callback wait is capped at five minutes; zero timeout is rejected before network access.
pub fn login_oidc<T: CloudTransport, B: Browser>(
    client: &CloudClient<T>,
    browser: &B,
    details: &OidcClientDetails,
    timeout: Duration,
) -> Result<(), CloudError> {
    if timeout.is_zero() {
        return Err(CloudError::Validation(
            "OIDC callback timeout must be positive".into(),
        ));
    }
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .map_err(|_| CloudError::Io("could not bind OIDC callback".into()))?;
    listener
        .set_nonblocking(false)
        .map_err(|_| CloudError::Io("could not configure OIDC callback".into()))?;
    let address = listener
        .local_addr()
        .map_err(|_| CloudError::Io("could not inspect OIDC callback".into()))?;
    let redirect_uri = format!("http://127.0.0.1:{}/oidc/callback", address.port());

    let state = Uuid::new_v4().to_string();
    let random_a = Uuid::new_v4();
    let random_b = Uuid::new_v4();
    let mut verifier_material = [
        random_a.as_bytes().as_slice(),
        random_b.as_bytes().as_slice(),
    ]
    .concat();
    let verifier = Zeroizing::new(URL_SAFE_NO_PAD.encode(&verifier_material));
    verifier_material.zeroize();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let start = OidcStartRequest {
        organization_id: details.organization_id,
        desktop: OidcDesktopRequest {
            client_kind: details.client_kind.clone(),
            client_id: details.client_id.clone(),
            device_name: details.device_name.clone(),
            redirect_uri,
            code_challenge: challenge,
            state: state.clone(),
        },
    };
    let authorization = client.begin_oidc(start)?;
    validate_authorization_url(&authorization.authorization_url)?;
    browser.open(&authorization.authorization_url)?;
    let callback = Zeroizing::new(receive_callback_with_timeout(
        &listener,
        address,
        &state,
        timeout.min(MAX_CALLBACK_TIMEOUT),
    )?);
    client.exchange_oidc(&callback, &verifier)
}

fn validate_authorization_url(value: &str) -> Result<(), CloudError> {
    let Some(remainder) = value.strip_prefix("https://") else {
        return Err(CloudError::InvalidUrl);
    };
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    let valid_authority = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split_once(']').is_some_and(|(host, suffix)| {
            host.parse::<Ipv6Addr>().is_ok()
                && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port))
        })
    } else {
        let (host, port) = authority
            .rsplit_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        !host.is_empty()
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
            && port.is_none_or(valid_port)
    };
    if authority.is_empty()
        || authority.contains('@')
        || !valid_authority
        || value.contains('#')
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(CloudError::InvalidUrl);
    }
    Ok(())
}

fn valid_port(value: &str) -> bool {
    value.parse::<u16>().is_ok_and(|port| port != 0)
}

fn receive_callback_with_timeout(
    listener: &TcpListener,
    expected_address: SocketAddr,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, CloudError> {
    listener
        .set_nonblocking(true)
        .map_err(|_| CloudError::Io("could not configure OIDC callback".into()))?;
    let deadline = std::time::Instant::now() + timeout;
    let mut invalid_callbacks = 0;
    loop {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !peer.ip().is_loopback() {
                    invalid_callbacks += 1;
                    if invalid_callbacks > MAX_INVALID_CALLBACKS {
                        return Err(CloudError::InvalidResponse);
                    }
                    continue;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(CloudError::Offline);
                }
                let io_timeout = remaining.min(Duration::from_secs(5));
                stream
                    .set_write_timeout(Some(io_timeout))
                    .map_err(|_| CloudError::Io("could not configure OIDC callback".into()))?;
                let outcome =
                    parse_callback(&mut stream, expected_address, expected_state, deadline);
                respond_callback(&mut stream, outcome.is_ok());
                match outcome {
                    Ok(code) => return Ok(code),
                    Err(error) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(CloudError::Offline);
                        }
                        invalid_callbacks += 1;
                        if invalid_callbacks > MAX_INVALID_CALLBACKS {
                            return Err(error);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(CloudError::Offline);
                }
                std::thread::sleep(
                    Duration::from_millis(25)
                        .min(deadline.saturating_duration_since(std::time::Instant::now())),
                );
            }
            Err(_) => return Err(CloudError::Io("OIDC callback failed".into())),
        }
    }
}

fn parse_callback(
    stream: &mut TcpStream,
    expected_address: SocketAddr,
    expected_state: &str,
    deadline: std::time::Instant,
) -> Result<String, CloudError> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(CloudError::Offline);
        }
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_secs(5))))
            .map_err(|_| CloudError::Io("could not configure OIDC callback".into()))?;
        let count = match stream.read(&mut chunk) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && std::time::Instant::now() >= deadline =>
            {
                return Err(CloudError::Offline);
            }
            Err(_) => return Err(CloudError::InvalidResponse),
        };
        if std::time::Instant::now() >= deadline {
            return Err(CloudError::Offline);
        }
        if count == 0 {
            return Err(CloudError::InvalidResponse);
        }
        if request.len().saturating_add(count) > MAX_CALLBACK_BYTES {
            return Err(CloudError::InvalidResponse);
        }
        let Some(read_bytes) = chunk.get(..count) else {
            return Err(CloudError::InvalidResponse);
        };
        request.extend_from_slice(read_bytes);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&request).map_err(|_| CloudError::InvalidResponse)?;
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(CloudError::InvalidResponse)?;
    if header_end.checked_add(4) != Some(request.len()) {
        return Err(CloudError::InvalidResponse);
    }
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(CloudError::InvalidResponse)?;
    if !request_line
        .strip_prefix("GET /oidc/callback?")
        .is_some_and(|value| value.ends_with(" HTTP/1.1"))
    {
        return Err(CloudError::InvalidResponse);
    }
    let mut host = None;
    let mut content_length_seen = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(CloudError::InvalidResponse)?;
        if name.eq_ignore_ascii_case("host") && host.replace(value.trim()).is_some() {
            return Err(CloudError::InvalidResponse);
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length_seen || value.trim() != "0" {
                return Err(CloudError::InvalidResponse);
            }
            content_length_seen = true;
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(CloudError::InvalidResponse);
        }
    }
    let expected_host = format!("127.0.0.1:{}", expected_address.port());
    if host != Some(expected_host.as_str()) {
        return Err(CloudError::InvalidResponse);
    }
    let query = request_line
        .strip_prefix("GET /oidc/callback?")
        .and_then(|v| v.strip_suffix(" HTTP/1.1"))
        .ok_or(CloudError::InvalidResponse)?;
    let mut values = std::collections::HashMap::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').ok_or(CloudError::InvalidResponse)?;
        let key = percent_decode(key)?;
        let value = percent_decode(value)?;
        if values.insert(key, value).is_some() {
            return Err(CloudError::InvalidResponse);
        }
    }
    let state = values.remove("state").ok_or(CloudError::InvalidResponse)?;
    if !constant_time_eq(state.as_bytes(), expected_state.as_bytes()) {
        return Err(CloudError::Unauthorized);
    }
    if values.contains_key("error") {
        return Err(CloudError::Unauthorized);
    }
    let code = values.remove("code").ok_or(CloudError::InvalidResponse)?;
    if code.is_empty() || code.len() > 4096 || !values.is_empty() {
        return Err(CloudError::InvalidResponse);
    }
    Ok(code)
}

fn percent_decode(value: &str) -> Result<String, CloudError> {
    let mut decoded = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes
            .get(index)
            .copied()
            .ok_or(CloudError::InvalidResponse)?
        {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index.checked_add(2).is_some_and(|last| last < bytes.len()) => {
                let next = index.checked_add(1).ok_or(CloudError::InvalidResponse)?;
                let last = index.checked_add(2).ok_or(CloudError::InvalidResponse)?;
                let hi = bytes
                    .get(next)
                    .copied()
                    .and_then(|b| (b as char).to_digit(16));
                let lo = bytes
                    .get(last)
                    .copied()
                    .and_then(|b| (b as char).to_digit(16));
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err(CloudError::InvalidResponse);
                };
                decoded.push(((hi << 4) | lo) as u8);
                index += 3;
            }
            b'%' => return Err(CloudError::InvalidResponse),
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| CloudError::InvalidResponse)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..right.len() {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn respond_callback(stream: &mut TcpStream, success: bool) {
    let (status, body) = if success {
        (
            "200 OK",
            "Authentication complete. Return to the application.",
        )
    } else {
        (
            "400 Bad Request",
            "Authentication failed. Return to the application.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn address() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 43123))
    }

    #[test]
    fn callback_accepts_exact_get_host_and_state() {
        let request =
            "GET /oidc/callback?code=abc&state=expected HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n"
                .to_owned();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
        assert!(listener.is_ok());
        if let Ok(listener) = listener {
            let target = listener.local_addr();
            assert!(target.is_ok());
            if let Ok(target) = target {
                let peer = TcpStream::connect(target);
                let accepted = listener.accept();
                assert!(peer.is_ok() && accepted.is_ok());
                if let (Ok(mut peer), Ok((mut stream, _))) = (peer, accepted) {
                    assert!(peer.write_all(request.as_bytes()).is_ok());
                    assert_eq!(
                        parse_callback(
                            &mut stream,
                            address(),
                            "expected",
                            std::time::Instant::now() + Duration::from_secs(2)
                        ),
                        Ok("abc".into())
                    );
                }
            }
        }
    }

    fn parse_test_request(request: &[u8]) -> Result<String, CloudError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| CloudError::Io("test listener failed".into()))?;
        let target = listener
            .local_addr()
            .map_err(|_| CloudError::Io("test listener failed".into()))?;
        let mut peer = TcpStream::connect(target)
            .map_err(|_| CloudError::Io("test connection failed".into()))?;
        let (mut stream, _) = listener
            .accept()
            .map_err(|_| CloudError::Io("test accept failed".into()))?;
        peer.write_all(request)
            .map_err(|_| CloudError::Io("test write failed".into()))?;
        drop(peer);
        parse_callback(
            &mut stream,
            address(),
            "expected",
            std::time::Instant::now() + Duration::from_secs(2),
        )
    }

    #[test]
    fn callback_rejects_truncated_oversized_and_duplicate_host_headers() {
        let truncated =
            b"GET /oidc/callback?code=abc&state=expected HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n";
        assert_eq!(
            parse_test_request(truncated),
            Err(CloudError::InvalidResponse)
        );

        let oversized = vec![b'a'; MAX_CALLBACK_BYTES + 1];
        assert_eq!(
            parse_test_request(&oversized),
            Err(CloudError::InvalidResponse)
        );

        let duplicate_host = b"GET /oidc/callback?code=abc&state=expected HTTP/1.1\r\nHost: 127.0.0.1:43123\r\nHost: 127.0.0.1:43123\r\n\r\n";
        assert_eq!(
            parse_test_request(duplicate_host),
            Err(CloudError::InvalidResponse)
        );
    }

    #[test]
    fn callback_listener_accepts_valid_request_after_thirty_two_invalid_requests() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
        assert!(listener.is_ok());
        let Ok(listener) = listener else {
            return;
        };
        let local = listener.local_addr();
        assert!(local.is_ok());
        let Ok(local) = local else {
            return;
        };
        let callback_listener = listener.try_clone();
        assert!(callback_listener.is_ok());
        let Ok(callback_listener) = callback_listener else {
            return;
        };
        let sender = std::thread::spawn(move || {
            for path in std::iter::repeat_n("/wrong?code=bad&state=expected", MAX_INVALID_CALLBACKS)
                .chain(std::iter::once("/oidc/callback?code=good&state=expected"))
            {
                let Ok(mut stream) = TcpStream::connect(local) else {
                    return false;
                };
                let request = format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                    local.port()
                );
                if stream.write_all(request.as_bytes()).is_err() {
                    return false;
                }
                let mut response = [0_u8; 128];
                let _ = stream.read(&mut response);
            }
            true
        });
        assert_eq!(
            receive_callback_with_timeout(
                &callback_listener,
                local,
                "expected",
                Duration::from_secs(5)
            ),
            Ok("good".into())
        );
        assert!(sender.join().is_ok_and(|sent| sent));
    }

    #[test]
    fn callback_listener_times_out_without_a_request() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
        assert!(listener.is_ok());
        if let Ok(listener) = listener {
            let local = listener.local_addr();
            assert!(local.is_ok());
            if let Ok(local) = local {
                assert_eq!(
                    receive_callback_with_timeout(
                        &listener,
                        local,
                        "expected",
                        Duration::from_millis(5)
                    ),
                    Err(CloudError::Offline)
                );
            }
        }
    }

    #[test]
    fn callback_slow_trickle_cannot_extend_absolute_timeout() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
        assert!(listener.is_ok());
        let Ok(listener) = listener else {
            return;
        };
        let local = listener.local_addr();
        assert!(local.is_ok());
        let Ok(local) = local else {
            return;
        };
        let callback_listener = listener.try_clone();
        assert!(callback_listener.is_ok());
        let Ok(callback_listener) = callback_listener else {
            return;
        };
        let sender = std::thread::spawn(move || {
            let Ok(mut stream) = TcpStream::connect(local) else {
                return;
            };
            let request = format!(
                "GET /oidc/callback?code=slow&state=expected HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                local.port()
            );
            for byte in request.bytes() {
                if stream.write_all(&[byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        let started = std::time::Instant::now();
        assert_eq!(
            receive_callback_with_timeout(
                &callback_listener,
                local,
                "expected",
                Duration::from_millis(180),
            ),
            Err(CloudError::Offline)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(sender.join().is_ok());
    }

    #[test]
    fn callback_rejects_wrong_path_host_state_and_duplicate_query() {
        for request in [
            "GET /wrong?code=abc&state=s HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n",
            "GET /oidc/callback?code=abc&state=s HTTP/1.1\r\nHost: localhost:43123\r\n\r\n",
            "GET /oidc/callback?code=abc&state=s&state=s HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n",
            "GET /oidc/callback?code=abc&state=bad HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n",
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
            assert!(listener.is_ok());
            if let Ok(listener) = listener {
                let target = listener.local_addr();
                assert!(target.is_ok());
                let mut peer = target
                    .ok()
                    .and_then(|target| TcpStream::connect(target).ok());
                let accepted = listener.accept();
                assert!(accepted.is_ok());
                if let (Some(mut peer), Ok((mut stream, _))) = (peer.take(), accepted) {
                    assert!(peer.write_all(request.as_bytes()).is_ok());
                    assert!(
                        parse_callback(
                            &mut stream,
                            address(),
                            "s",
                            std::time::Instant::now() + Duration::from_secs(2)
                        )
                        .is_err()
                    );
                }
            }
        }
    }

    #[test]
    fn authorization_url_requires_https_without_credentials_or_fragment() {
        assert!(validate_authorization_url("https://idp.example/authorize").is_ok());
        for value in [
            "http://idp.example/authorize",
            "https://user@idp.example/authorize",
            "https://idp.example/authorize#fragment",
            "https://idp.example:bad/authorize",
            "https://:443/authorize",
        ] {
            assert_eq!(
                validate_authorization_url(value),
                Err(CloudError::InvalidUrl)
            );
        }
    }

    struct MockTransport {
        redirect_uri: Arc<Mutex<Option<String>>>,
        callback_state: Arc<Mutex<Option<String>>>,
        requests: Mutex<Vec<craftcloud_client::Request>>,
    }

    impl CloudTransport for MockTransport {
        fn send(
            &self,
            request: craftcloud_client::Request,
        ) -> Result<craftcloud_client::Response, CloudError> {
            if let Ok(mut requests) = self.requests.lock() {
                requests.push(request.clone());
            }
            let body = if request.path.ends_with("/oidc/start") {
                let parsed: serde_json::Value = serde_json::from_slice(&request.body)
                    .map_err(|_| CloudError::InvalidResponse)?;
                let redirect = parsed
                    .get("desktop")
                    .and_then(|desktop| desktop.get("redirect_uri"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or(CloudError::InvalidResponse)?
                    .to_owned();
                *self.redirect_uri.lock().map_err(|_| CloudError::Offline)? = Some(redirect);
                let state = parsed
                    .get("desktop")
                    .and_then(|desktop| desktop.get("state"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or(CloudError::InvalidResponse)?
                    .to_owned();
                *self
                    .callback_state
                    .lock()
                    .map_err(|_| CloudError::Offline)? = Some(state);
                r#"{"authorization_url":"https://idp.example/authorize"}"#
            } else {
                r#"{"access_token":"access","access_expires_at":"2099-01-01T00:00:00Z","refresh_token":"refresh","refresh_expires_at":"2099-01-01T00:00:00Z","device_session_id":"00000000-0000-0000-0000-000000000001","user":{"user_id":"00000000-0000-0000-0000-000000000002","email":"user@example.test","display_name":"User","instance_role":null}}"#
            };
            Ok(craftcloud_client::Response {
                status: 200,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
            })
        }
    }

    struct MockBrowser {
        redirect_uri: Arc<Mutex<Option<String>>>,
        callback_state: Arc<Mutex<Option<String>>>,
    }

    impl Browser for MockBrowser {
        fn open(&self, authorization_url: &str) -> Result<(), CloudError> {
            validate_authorization_url(authorization_url)?;
            let redirect = self
                .redirect_uri
                .lock()
                .map_err(|_| CloudError::Offline)?
                .clone()
                .ok_or(CloudError::InvalidResponse)?;
            let state = self
                .callback_state
                .lock()
                .map_err(|_| CloudError::Offline)?
                .clone()
                .ok_or(CloudError::InvalidResponse)?;
            let port = redirect
                .rsplit(':')
                .next()
                .and_then(|p| p.split('/').next())
                .and_then(|p| p.parse::<u16>().ok())
                .ok_or(CloudError::InvalidResponse)?;
            let mut stream =
                TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map_err(|_| CloudError::Offline)?;
            let request = format!(
                "GET /oidc/callback?code=mock-code&state={} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n",
                state
            );
            stream
                .write_all(request.as_bytes())
                .map_err(|_| CloudError::Offline)
        }
    }

    type TestClient = (
        CloudClient<MockTransport>,
        Arc<Mutex<Option<String>>>,
        Arc<Mutex<Option<String>>>,
    );
    fn test_client() -> Option<TestClient> {
        let redirect_uri = Arc::new(Mutex::new(None));
        let callback_state = Arc::new(Mutex::new(None));
        let client = CloudClient::new(
            "https://cloud.example",
            MockTransport {
                redirect_uri: Arc::clone(&redirect_uri),
                callback_state: Arc::clone(&callback_state),
                requests: Mutex::new(Vec::new()),
            },
        )
        .ok()?;
        Some((client, redirect_uri, callback_state))
    }

    fn details() -> OidcClientDetails {
        OidcClientDetails {
            organization_id: Uuid::from_u128(9),
            client_kind: "desktop".into(),
            client_id: "test-app".into(),
            device_name: "test device".into(),
        }
    }

    #[test]
    fn oidc_flow_uses_start_and_exchange_once_and_stores_tokens() {
        let setup = test_client();
        assert!(setup.is_some());
        let Some((client, redirect_uri, callback_state)) = setup else {
            return;
        };
        let token_store = Arc::new(RecordingTokenStore(Mutex::new(Vec::new())));
        assert!(client.set_refresh_token_store(token_store.clone()).is_ok());
        let browser = MockBrowser {
            redirect_uri,
            callback_state,
        };
        assert!(login_oidc(&client, &browser, &details(), Duration::from_secs(2)).is_ok());
        assert!(client.is_authenticated());
        let requests = client.transport().requests.lock();
        assert!(requests.as_ref().is_ok_and(|requests| requests.len() == 2));
        let Ok(requests) = requests else {
            return;
        };
        let start = requests
            .first()
            .and_then(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok());
        let exchange = requests
            .get(1)
            .and_then(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok());
        assert!(start.is_some() && exchange.is_some());
        let (Some(start), Some(exchange)) = (start, exchange) else {
            return;
        };
        let verifier = exchange
            .get("code_verifier")
            .and_then(serde_json::Value::as_str);
        let challenge = start
            .get("desktop")
            .and_then(|desktop| desktop.get("code_challenge"))
            .and_then(serde_json::Value::as_str);
        let state = start
            .get("desktop")
            .and_then(|desktop| desktop.get("state"))
            .and_then(serde_json::Value::as_str);
        assert!(verifier.is_some() && challenge.is_some() && state.is_some());
        let (Some(verifier), Some(challenge), Some(state)) = (verifier, challenge, state) else {
            return;
        };
        let expected_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let redirect_uri = start
            .get("desktop")
            .and_then(|desktop| desktop.get("redirect_uri"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(
            start,
            serde_json::json!({
                "organization_id": "00000000-0000-0000-0000-000000000009",
                "desktop": {
                    "client_kind": "desktop",
                    "client_id": "test-app",
                    "device_name": "test device",
                    "redirect_uri": redirect_uri,
                    "code_challenge": expected_challenge.clone(),
                    "state": state,
                }
            })
        );
        assert!(verifier.len() >= 43 && verifier.len() <= 128);
        assert!(
            verifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
        assert_eq!(challenge, expected_challenge);
        assert_eq!(
            exchange,
            serde_json::json!({"code":"mock-code", "code_verifier": verifier})
        );
        assert!(redirect_uri.is_some_and(
            |value| value.starts_with("http://127.0.0.1:") && value.ends_with("/oidc/callback")
        ));
        assert!(
            requests
                .first()
                .is_some_and(|request| request.method == "POST"
                    && request.path.ends_with("/api/v1/auth/oidc/start"))
        );
        assert!(
            requests
                .get(1)
                .is_some_and(|request| request.method == "POST"
                    && request.path.ends_with("/api/v1/auth/oidc/device/exchange"))
        );
        let stored = token_store.0.lock();
        assert!(stored.as_ref().is_ok_and(|values| {
            values
                .last()
                .is_some_and(|value| value.as_deref() == Some("refresh"))
        }));
    }

    #[test]
    fn oidc_state_failure_never_exchanges_code() {
        let setup = test_client();
        assert!(setup.is_some());
        let Some((client, redirect_uri, _callback_state)) = setup else {
            return;
        };
        let browser = MockBrowser {
            redirect_uri,
            callback_state: Arc::new(Mutex::new(Some("attacker-state".into()))),
        };
        assert_eq!(
            login_oidc(&client, &browser, &details(), Duration::from_millis(300)),
            Err(CloudError::Offline)
        );
        assert!(!client.is_authenticated());
        let requests = client.transport().requests.lock();
        assert!(requests.as_ref().is_ok_and(|requests| {
            requests.len() == 1
                && requests
                    .first()
                    .is_some_and(|request| request.path.ends_with("/api/v1/auth/oidc/start"))
        }));
    }

    struct FailingBrowser;

    impl Browser for FailingBrowser {
        fn open(&self, _: &str) -> Result<(), CloudError> {
            Err(CloudError::Io("mock browser failure".into()))
        }
    }

    #[test]
    fn browser_failure_preserves_existing_session_and_refresh_token() {
        let setup = test_client();
        assert!(setup.is_some());
        let Some((client, _redirect_uri, _callback_state)) = setup else {
            return;
        };
        let token_store = Arc::new(RecordingTokenStore(Mutex::new(Vec::new())));
        assert!(client.set_refresh_token_store(token_store.clone()).is_ok());
        assert!(
            client
                .set_tokens(craftcloud_client::DeviceTokens {
                    access_token: craftcloud_client::Secret::new("old-access"),
                    refresh_token: craftcloud_client::Secret::new("old-refresh"),
                    expires_at: String::new(),
                })
                .is_ok()
        );

        assert_eq!(
            login_oidc(&client, &FailingBrowser, &details(), Duration::from_secs(2)),
            Err(CloudError::Io("mock browser failure".into()))
        );
        assert!(client.is_authenticated());
        let refresh = client.refresh_token();
        assert!(refresh.is_ok_and(|token| token.expose() == "old-refresh"));
        let persisted = token_store.0.lock();
        assert!(persisted.as_ref().is_ok_and(|values| {
            values
                .last()
                .is_some_and(|value| value.as_deref() == Some("old-refresh"))
        }));
        let requests = client.transport().requests.lock();
        assert!(requests.as_ref().is_ok_and(|requests| requests.len() == 1));
    }

    #[test]
    fn zero_timeout_is_rejected_before_start_request() {
        let setup = test_client();
        assert!(setup.is_some());
        let Some((client, _redirect_uri, _callback_state)) = setup else {
            return;
        };
        assert_eq!(
            login_oidc(&client, &FailingBrowser, &details(), Duration::ZERO),
            Err(CloudError::Validation(
                "OIDC callback timeout must be positive".into()
            ))
        );
        let requests = client.transport().requests.lock();
        assert!(requests.as_ref().is_ok_and(|requests| requests.is_empty()));
    }

    struct RecordingTokenStore(Mutex<Vec<Option<String>>>);

    impl craftcloud_client::RefreshTokenStore for RecordingTokenStore {
        fn store(&self, refresh: Option<&craftcloud_client::Secret>) -> Result<(), CloudError> {
            let mut values = self.0.lock().map_err(|_| CloudError::Offline)?;
            values.push(refresh.map(|token| token.expose().to_owned()));
            Ok(())
        }
    }
}
