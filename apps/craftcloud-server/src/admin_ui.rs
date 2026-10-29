//! Static delivery of the browser admin UI under `/admin` (ADR-0015).
//!
//! The directory is the build output of `apps/craftcloud-admin` (`dist/`) and
//! is configured with `CRAFTCLOUD_ADMIN_DIR`. It is read completely into
//! memory **once, at startup** ([`load`]); requests are answered from that
//! snapshot, so nothing on disk is touched while serving (no race between a
//! path check and a read). Without the variable nothing is mounted and
//! `/admin` answers the normal JSON 404.
//!
//! Rules:
//! - Loading refuses what could escape or exhaust the server: names with
//!   unsafe characters, links that leave the directory, directory links, more
//!   than [`MAX_FILES`] files, files over [`MAX_FILE_BYTES`] and a total over
//!   [`MAX_TOTAL_BYTES`]. Files whose name starts with a dot are skipped.
//! - Only `GET` and `HEAD`; other methods answer a JSON 405. Every answer
//!   (also 404, 405 and redirects) carries the same strict security headers.
//! - Requested paths are decoded once and checked segment by segment; a
//!   segment with `..`, `.`, a backslash, a colon, control characters, a
//!   leading dot or a Windows device name makes the request a 404. Only exact
//!   names that were loaded can be served.
//! - Single-page-application fallback: a path that names no file and whose
//!   last segment has no extension answers `index.html`. A missing file with
//!   an extension (a stale asset) is a real 404.
//! - Hashed build assets (`/admin/assets/*`) are immutable; everything else,
//!   including `index.html`, is `no-store`.
//! - Invitation links created by the API point at `/invite#token=...`; that
//!   path redirects to `/admin/invite` (browsers keep the fragment). Password
//!   reset links point at `/reset-password#token=...` and redirect to
//!   `/admin/reset-password` the same way.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

/// Largest single file accepted at startup.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Largest total size of all files accepted at startup.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Largest number of files accepted at startup.
pub const MAX_FILES: usize = 10_000;
const MAX_DEPTH: usize = 16;
const MAX_PATH_BYTES: usize = 1024;
const MAX_SEGMENTS: usize = 32;

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_STORE: &str = "no-store";

/// Limits applied while loading; tests use smaller ones.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_files: MAX_FILES,
        }
    }
}

struct Asset {
    bytes: Bytes,
    content_type: &'static str,
}

/// The loaded UI: every file by its path relative to the directory.
pub struct Assets {
    files: HashMap<String, Asset>,
}

/// Reads the whole directory into memory (see the module documentation).
pub fn load(dir: &Path) -> Result<Assets, String> {
    load_with_limits(dir, Limits::default())
}

pub fn load_with_limits(dir: &Path, limits: Limits) -> Result<Assets, String> {
    let root = fs::canonicalize(dir).map_err(|err| {
        format!(
            "cannot open the admin UI directory {}: {err}",
            dir.display()
        )
    })?;
    if !root.is_dir() {
        return Err(format!(
            "the admin UI directory {} is not a directory",
            dir.display()
        ));
    }
    let mut files = HashMap::new();
    let mut total = 0_u64;
    walk(&root, &root, "", 0, limits, &mut total, &mut files)?;
    if !files.contains_key("index.html") {
        return Err(format!(
            "the admin UI directory {} has no index.html (build it with `cargo xtask web`)",
            dir.display()
        ));
    }
    Ok(Assets { files })
}

fn walk(
    root: &Path,
    dir: &Path,
    prefix: &str,
    depth: usize,
    limits: Limits,
    total: &mut u64,
    files: &mut HashMap<String, Asset>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "{} is nested deeper than {MAX_DEPTH} levels",
            dir.display()
        ));
    }
    let entries =
        fs::read_dir(dir).map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{} has a name that is not valid UTF-8", path.display()))?;
        if name.starts_with('.') {
            continue;
        }
        if !segment_is_safe(&name) {
            return Err(format!("{} has an unsafe file name", path.display()));
        }
        let link = fs::symlink_metadata(&path)
            .map_err(|err| format!("cannot inspect {}: {err}", path.display()))?;
        if link.file_type().is_symlink() {
            let target = fs::canonicalize(&path)
                .map_err(|err| format!("cannot resolve link {}: {err}", path.display()))?;
            if !target.starts_with(root) {
                return Err(format!(
                    "{} is a link that leads outside the admin UI directory",
                    path.display()
                ));
            }
        }
        // `metadata` follows links that stay inside the directory.
        let meta = fs::metadata(&path)
            .map_err(|err| format!("cannot inspect {}: {err}", path.display()))?;
        let key = format!("{prefix}{name}");
        if meta.is_dir() {
            if link.file_type().is_symlink() {
                return Err(format!(
                    "{} is a directory link, which is not supported",
                    path.display()
                ));
            }
            walk(
                root,
                &path,
                &format!("{key}/"),
                depth + 1,
                limits,
                total,
                files,
            )?;
        } else if meta.is_file() {
            if files.len() >= limits.max_files {
                return Err(format!(
                    "more than {} files in the admin UI directory",
                    limits.max_files
                ));
            }
            if meta.len() > limits.max_file_bytes {
                return Err(format!(
                    "{} is larger than {} bytes",
                    path.display(),
                    limits.max_file_bytes
                ));
            }
            *total += meta.len();
            if *total > limits.max_total_bytes {
                return Err(format!(
                    "the admin UI directory is larger than {} bytes",
                    limits.max_total_bytes
                ));
            }
            let bytes =
                fs::read(&path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
            files.insert(
                key,
                Asset {
                    bytes: Bytes::from(bytes),
                    content_type: content_type(&name),
                },
            );
        } else {
            return Err(format!(
                "{} is neither a file nor a directory",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Builds the router for `/admin` (and the `/invite` redirect). Wrap it with
/// `craftcloud_api::with_request_context` for request ids and tracing.
pub fn router(assets: Assets) -> Router {
    Router::new()
        .route("/admin", get(redirect_to_base))
        .route("/admin/", get(serve_index))
        .route("/admin/{*path}", get(serve_path))
        .route("/invite", get(redirect_invite))
        .route("/reset-password", get(redirect_reset_password))
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(Arc::new(assets))
}

async fn redirect_to_base() -> Response {
    redirect("/admin/")
}

async fn redirect_invite() -> Response {
    // The token lives in the fragment, which the browser carries over.
    redirect("/admin/invite")
}

async fn redirect_reset_password() -> Response {
    // Same as the invitation link: the token stays in the fragment.
    redirect("/admin/reset-password")
}

fn redirect(location: &'static str) -> Response {
    let mut response = StatusCode::TEMPORARY_REDIRECT.into_response();
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static(location));
    finish(response, NO_STORE)
}

async fn method_not_allowed(headers: HeaderMap) -> Response {
    let request_id: String = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .collect()
        })
        .unwrap_or_default();
    let id = if request_id.is_empty() {
        "null".to_owned()
    } else {
        format!("\"{request_id}\"")
    };
    let body = format!(
        "{{\"error\":{{\"code\":\"METHOD_NOT_ALLOWED\",\"message\":\"The HTTP method is not allowed for this resource.\",\"request_id\":{id}}}}}"
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
    finish(response, NO_STORE)
}

async fn serve_index(State(assets): State<Arc<Assets>>) -> Response {
    lookup(&assets, &[])
}

async fn serve_path(State(assets): State<Arc<Assets>>, uri: Uri) -> Response {
    let relative = uri.path().strip_prefix("/admin/").unwrap_or("");
    match parse_path(relative) {
        Some(segments) => lookup(&assets, &segments),
        None => not_found(),
    }
}

fn lookup(assets: &Assets, segments: &[String]) -> Response {
    let index = || assets.files.get("index.html");
    let found = if segments.is_empty() {
        index().map(|asset| (asset, false))
    } else {
        let key = segments.join("/");
        match assets.files.get(&key) {
            Some(asset) => Some((asset, segments.first().is_some_and(|s| s == "assets"))),
            // A missing file with an extension is a real 404, never HTML.
            None if segments.last().is_some_and(|last| last.contains('.')) => None,
            None => index().map(|asset| (asset, false)),
        }
    };
    match found {
        Some((asset, immutable)) => {
            let mut response = Response::new(Body::from(asset.bytes.clone()));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(asset.content_type),
            );
            finish(response, if immutable { IMMUTABLE } else { NO_STORE })
        }
        None => not_found(),
    }
}

fn not_found() -> Response {
    finish(StatusCode::NOT_FOUND.into_response(), NO_STORE)
}

/// Adds the security headers every `/admin` answer carries.
fn finish(mut response: Response, cache_control: &'static str) -> Response {
    let headers = response.headers_mut();
    let fixed: [(HeaderName, &'static str); 8] = [
        (header::CACHE_CONTROL, cache_control),
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
        (
            HeaderName::from_static("permissions-policy"),
            "camera=(), microphone=(), geolocation=()",
        ),
    ];
    for (name, value) in fixed {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Percent-decodes `raw` once; `None` for malformed escapes, NUL or invalid UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let hi = hex_value(*bytes.get(index + 1)?)?;
            let lo = hex_value(*bytes.get(index + 2)?)?;
            out.push(hi * 16 + lo);
            index += 3;
        } else {
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_windows_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|digit| matches!(digit.as_bytes(), [b'1'..=b'9']))
        })
}

fn segment_is_safe(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.starts_with('.')
        && !segment.ends_with(['.', ' '])
        && !segment
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | '/' | ':'))
        && !is_windows_device_name(segment)
}

/// Splits a request path (without the `/admin/` prefix) into safe segments.
/// `None` means the request is refused. A trailing slash is allowed.
fn parse_path(raw: &str) -> Option<Vec<String>> {
    if raw.len() > MAX_PATH_BYTES {
        return None;
    }
    let decoded = percent_decode(raw)?;
    let trimmed = decoded.strip_suffix('/').unwrap_or(&decoded);
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    let segments: Vec<String> = trimmed.split('/').map(str::to_owned).collect();
    if segments.len() > MAX_SEGMENTS || !segments.iter().all(|s| segment_is_safe(s)) {
        return None;
    }
    Some(segments)
}

fn content_type(name: &str) -> &'static str {
    let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    match extension.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    /// Temp directory removed on drop; avoids a `tempfile` dependency.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "craftcloud-admin-ui-test-{}-{tag}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const INDEX: &str = "<!doctype html><title>admin</title>";

    /// `<base>/dist` served as the UI, `<base>/secret.txt` outside of it.
    struct Fixture {
        base: TempDir,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let base = TempDir::new(tag);
            let dist = base.0.join("dist");
            fs::create_dir_all(dist.join("assets")).unwrap();
            fs::write(dist.join("index.html"), INDEX).unwrap();
            fs::write(dist.join("assets").join("app-abc123.js"), "console.log(1)").unwrap();
            fs::write(dist.join("assets").join("app-abc123.css"), "body{}").unwrap();
            fs::write(dist.join("robots.txt"), "User-agent: *").unwrap();
            fs::write(dist.join(".hidden"), "hidden").unwrap();
            fs::write(base.0.join("secret.txt"), "TOP-SECRET").unwrap();
            Self { base }
        }

        fn dist(&self) -> PathBuf {
            self.base.0.join("dist")
        }
    }

    struct Running {
        addr: SocketAddr,
        stop: Option<oneshot::Sender<()>>,
        handle: tokio::task::JoinHandle<Result<crate::server::Shutdown, String>>,
    }

    async fn start(dir: &Path) -> Running {
        start_with(Some(dir)).await
    }

    async fn start_with(dir: Option<&Path>) -> Running {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, rx) = oneshot::channel::<()>();
        let api = craftcloud_api::router();
        let app = match dir {
            Some(dir) => api.merge(craftcloud_api::with_request_context(
                router(load(dir).unwrap()),
                &craftcloud_api::ApiConfig::default(),
            )),
            None => api,
        };
        let handle = tokio::spawn(crate::server::serve(
            listener,
            app,
            async move {
                let _ = rx.await;
            },
            Duration::from_secs(2),
        ));
        Running {
            addr,
            stop: Some(stop),
            handle,
        }
    }

    impl Running {
        async fn stop(mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), self.handle).await;
        }
    }

    struct Reply {
        status: u16,
        head: String,
        body: String,
    }

    impl Reply {
        fn header(&self, name: &str) -> Option<String> {
            let prefix = format!("{}:", name.to_ascii_lowercase());
            self.head.lines().find_map(|line| {
                line.to_ascii_lowercase()
                    .starts_with(&prefix)
                    .then(|| line.split_once(':').map(|(_, v)| v.trim().to_owned()))
                    .flatten()
            })
        }
    }

    /// Sends the request target byte for byte (no client-side normalization).
    async fn raw(addr: SocketAddr, method: &str, target: &str) -> Reply {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request =
            format!("{method} {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buffer = Vec::new();
        stream.read_to_end(&mut buffer).await.unwrap();
        let text = String::from_utf8_lossy(&buffer).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Reply {
            status,
            head: head.to_owned(),
            body: body.to_owned(),
        }
    }

    fn assert_security_headers(reply: &Reply) {
        let csp = reply.header("content-security-policy").unwrap();
        assert!(csp.contains("default-src 'self'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(!csp.contains("unsafe-inline"), "{csp}");
        assert!(!csp.contains("unsafe-eval"), "{csp}");
        assert_eq!(reply.header("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(reply.header("referrer-policy").unwrap(), "same-origin");
        assert_eq!(reply.header("x-frame-options").unwrap(), "DENY");
        assert_eq!(
            reply.header("cross-origin-opener-policy").unwrap(),
            "same-origin"
        );
        assert!(reply.header("x-request-id").is_some(), "request id missing");
    }

    #[tokio::test]
    async fn serves_index_assets_and_static_files_with_strict_headers() {
        let fixture = Fixture::new("serve");
        let running = start(&fixture.dist()).await;

        for target in ["/admin/", "/admin/index.html"] {
            let reply = raw(running.addr, "GET", target).await;
            assert_eq!(reply.status, 200, "{target}");
            assert_eq!(reply.body, INDEX);
            assert_eq!(
                reply.header("content-type").unwrap(),
                "text/html; charset=utf-8"
            );
            assert_eq!(reply.header("cache-control").unwrap(), "no-store");
            assert_security_headers(&reply);
        }

        let js = raw(running.addr, "GET", "/admin/assets/app-abc123.js").await;
        assert_eq!(js.status, 200);
        assert_eq!(js.body, "console.log(1)");
        assert_eq!(
            js.header("content-type").unwrap(),
            "text/javascript; charset=utf-8"
        );
        // The request-context layers must not force the API's `no-store`.
        assert_eq!(
            js.header("cache-control").unwrap(),
            "public, max-age=31536000, immutable"
        );
        assert_security_headers(&js);

        let css = raw(running.addr, "GET", "/admin/assets/app-abc123.css").await;
        assert_eq!(
            css.header("content-type").unwrap(),
            "text/css; charset=utf-8"
        );

        let robots = raw(running.addr, "GET", "/admin/robots.txt").await;
        assert_eq!(robots.status, 200);
        assert_eq!(robots.header("cache-control").unwrap(), "no-store");

        let head = raw(running.addr, "HEAD", "/admin/").await;
        assert_eq!(head.status, 200);
        assert_eq!(head.body, "");
        assert_security_headers(&head);
        running.stop().await;
    }

    #[tokio::test]
    async fn the_snapshot_is_served_from_memory() {
        let fixture = Fixture::new("memory");
        let running = start(&fixture.dist()).await;
        fs::remove_dir_all(fixture.dist()).unwrap();
        let reply = raw(running.addr, "GET", "/admin/").await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, INDEX);
        running.stop().await;
    }

    #[tokio::test]
    async fn spa_fallback_only_for_extensionless_paths_under_admin() {
        let fixture = Fixture::new("spa");
        let running = start(&fixture.dist()).await;

        for target in [
            "/admin/projects",
            "/admin/projects/123/files",
            "/admin/invite",
            "/admin/projects/",
            "/admin/assets",
        ] {
            let reply = raw(running.addr, "GET", target).await;
            assert_eq!(reply.status, 200, "{target}");
            assert_eq!(reply.body, INDEX, "{target}");
            assert_eq!(
                reply.header("cache-control").unwrap(),
                "no-store",
                "{target}"
            );
            assert_security_headers(&reply);
        }
        // A missing file with an extension is a real 404, never HTML.
        for target in ["/admin/assets/old-deadbeef.js", "/admin/missing.css"] {
            let reply = raw(running.addr, "GET", target).await;
            assert_eq!(reply.status, 404, "{target}");
            assert!(!reply.body.contains("admin"), "{target}");
            assert_security_headers(&reply);
        }
        // Outside /admin nothing falls back to the UI: the API's JSON 404 answers.
        for target in ["/", "/projects", "/administrator", "/api/v1/nothing"] {
            let reply = raw(running.addr, "GET", target).await;
            assert_eq!(reply.status, 404, "{target}");
            assert!(reply.body.contains("NOT_FOUND"), "{target}: {}", reply.body);
        }
        // The API keeps working next to the UI.
        let live = raw(running.addr, "GET", "/health/live").await;
        assert_eq!(live.status, 200);
        running.stop().await;
    }

    #[tokio::test]
    async fn only_get_and_head_and_redirects() {
        let fixture = Fixture::new("methods");
        let running = start(&fixture.dist()).await;

        for method in ["POST", "PUT", "DELETE", "PATCH"] {
            for target in ["/admin/", "/admin/projects", "/admin"] {
                let reply = raw(running.addr, method, target).await;
                assert_eq!(reply.status, 405, "{method} {target}");
                assert!(
                    reply.body.contains("\"code\":\"METHOD_NOT_ALLOWED\""),
                    "{method} {target}: {}",
                    reply.body
                );
                assert_eq!(
                    reply.header("content-type").unwrap(),
                    "application/json",
                    "{method} {target}"
                );
                assert_eq!(reply.header("allow").unwrap(), "GET, HEAD");
                assert_security_headers(&reply);
            }
        }
        let base = raw(running.addr, "GET", "/admin").await;
        assert_eq!(base.status, 307);
        assert_eq!(base.header("location").unwrap(), "/admin/");
        assert_security_headers(&base);

        let invite = raw(running.addr, "GET", "/invite").await;
        assert_eq!(invite.status, 307);
        assert_eq!(invite.header("location").unwrap(), "/admin/invite");
        assert_security_headers(&invite);

        let reset = raw(running.addr, "GET", "/reset-password").await;
        assert_eq!(reset.status, 307);
        assert_eq!(reset.header("location").unwrap(), "/admin/reset-password");
        assert_security_headers(&reset);
        running.stop().await;
    }

    #[tokio::test]
    async fn path_traversal_and_odd_paths_never_leave_the_snapshot() {
        let fixture = Fixture::new("traversal");
        let running = start(&fixture.dist()).await;

        let attempts = [
            "/admin/../secret.txt",
            "/admin/assets/../../secret.txt",
            "/admin/%2e%2e/secret.txt",
            "/admin/%2E%2E/secret.txt",
            "/admin/assets/%2e%2e/%2e%2e/secret.txt",
            "/admin/..%2fsecret.txt",
            "/admin/%2e%2e%2fsecret.txt",
            "/admin/assets%2f..%2f..%2fsecret.txt",
            "/admin/..%5csecret.txt",
            "/admin/..\\secret.txt",
            "/admin/%5c..%5csecret.txt",
            "/admin//secret.txt",
            "/admin//etc/passwd",
            "/admin/%2fetc%2fpasswd",
            "/admin/C:/Windows/win.ini",
            "/admin/C%3A%5CWindows%5Cwin.ini",
            "/admin/%00",
            "/admin/index.html%00.js",
            "/admin/%ff",
            "/admin/%zz",
            "/admin/%",
            "/admin/.hidden",
            "/admin/assets/.hidden",
            "/admin/NUL",
            "/admin/con.txt",
            "/admin/COM1",
            "/admin/index.html.",
            "/admin/assets%20",
            "/admin/robots.txt::$DATA",
        ];
        for target in attempts {
            let reply = raw(running.addr, "GET", target).await;
            assert!(
                !reply.body.contains("TOP-SECRET") && !reply.body.contains("hidden"),
                "{target} leaked a file"
            );
            assert!(
                reply.status == 404 || reply.status == 400 || reply.status == 307,
                "{target} answered {} with body {:?}",
                reply.status,
                reply.body
            );
            assert_ne!(reply.body, INDEX, "{target} must not fall back to the UI");
        }
        running.stop().await;
    }

    /// Creates a directory/file link (a junction on Windows when symbolic links
    /// are not allowed); `false` when the platform refuses.
    fn try_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            if target.is_dir() {
                // A junction needs no special privilege, unlike a symbolic link.
                std::os::windows::fs::symlink_dir(target, link).is_ok()
                    || std::process::Command::new("cmd")
                        .args(["/C", "mklink", "/J"])
                        .arg(link)
                        .arg(target)
                        .output()
                        .is_ok_and(|out| out.status.success())
            } else {
                std::os::windows::fs::symlink_file(target, link).is_ok()
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            false
        }
    }

    #[test]
    fn loading_refuses_links_that_leave_the_directory() {
        let fixture = Fixture::new("symlink-out");
        let dist = fixture.dist();
        let made_file = try_symlink(&fixture.base.0.join("secret.txt"), &dist.join("escape.txt"));
        if made_file {
            let err = load(&dist).err().unwrap();
            assert!(err.contains("leads outside"), "{err}");
            fs::remove_file(dist.join("escape.txt")).unwrap();
        } else {
            eprintln!("SKIPPED file link: this platform/user cannot create symbolic links");
        }
        let made_dir = try_symlink(&fixture.base.0, &dist.join("escape-dir"));
        if made_dir {
            let err = load(&dist).err().unwrap();
            assert!(err.contains("leads outside"), "{err}");
            // `remove_dir` removes the link itself, not its target.
            fs::remove_dir(dist.join("escape-dir")).unwrap();
        } else {
            eprintln!("SKIPPED directory link: this platform/user cannot create links");
        }
        assert!(load(&dist).is_ok());
        assert!(fixture.base.0.join("secret.txt").is_file());
    }

    #[tokio::test]
    async fn a_link_inside_the_directory_is_served_as_a_copy() {
        let fixture = Fixture::new("symlink-in");
        let dist = fixture.dist();
        if !try_symlink(&dist.join("robots.txt"), &dist.join("inside-link.txt")) {
            eprintln!("SKIPPED: this platform/user cannot create symbolic links");
            return;
        }
        let running = start(&dist).await;
        let reply = raw(running.addr, "GET", "/admin/inside-link.txt").await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, "User-agent: *");
        running.stop().await;
    }

    #[test]
    fn load_refuses_unusable_directories_and_oversized_content() {
        let fixture = Fixture::new("refuse");
        let missing = fixture.base.0.join("nope");
        assert!(load(&missing).err().unwrap().contains("cannot open"));
        let file = fixture.base.0.join("secret.txt");
        assert!(load(&file).err().unwrap().contains("not a directory"));
        let empty = fixture.base.0.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(load(&empty).err().unwrap().contains("no index.html"));

        let dist = fixture.dist();
        let small = Limits {
            max_file_bytes: 20,
            ..Limits::default()
        };
        // js file is 14 bytes, index is 36 bytes: the index exceeds the cap.
        assert!(
            load_with_limits(&dist, small)
                .err()
                .unwrap()
                .contains("larger than 20 bytes")
        );
        let total = Limits {
            max_total_bytes: 50,
            ..Limits::default()
        };
        assert!(
            load_with_limits(&dist, total)
                .err()
                .unwrap()
                .contains("directory is larger than 50 bytes")
        );
        let count = Limits {
            max_files: 2,
            ..Limits::default()
        };
        assert!(
            load_with_limits(&dist, count)
                .err()
                .unwrap()
                .contains("more than 2 files")
        );
        // Normal limits accept the fixture; the dotfile is skipped.
        let assets = load(&dist).unwrap();
        assert!(!assets.files.contains_key(".hidden"));
        assert!(assets.files.contains_key("assets/app-abc123.js"));
    }

    #[cfg(unix)]
    #[test]
    fn load_refuses_unsafe_names() {
        let fixture = Fixture::new("names");
        let dist = fixture.dist();
        fs::write(dist.join("a\\b.js"), "x").unwrap();
        assert!(load(&dist).err().unwrap().contains("unsafe file name"));
    }

    #[tokio::test]
    async fn without_a_directory_nothing_is_mounted() {
        let running = start_with(None).await;
        for target in [
            "/admin",
            "/admin/",
            "/admin/assets/app.js",
            "/invite",
            "/reset-password",
        ] {
            let reply = raw(running.addr, "GET", target).await;
            assert_eq!(reply.status, 404, "{target}");
            assert!(reply.body.contains("NOT_FOUND"), "{target}: {}", reply.body);
        }
        running.stop().await;
    }

    #[test]
    fn path_parsing() {
        assert_eq!(parse_path(""), Some(vec![]));
        assert_eq!(parse_path("a/b/"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(parse_path("a//b"), None);
        assert_eq!(parse_path("a%2Fb"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(parse_path("%2e%2e"), None);
        assert_eq!(parse_path("caf%C3%A9"), Some(vec!["café".into()]));
        assert!(parse_path(&"a/".repeat(40)).is_none());
        assert!(parse_path(&"a".repeat(2000)).is_none());
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type("a.JS"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("a.map"), "application/json");
        assert_eq!(content_type("noext"), "application/octet-stream");
        assert_eq!(content_type("a.exe"), "application/octet-stream");
    }
}
