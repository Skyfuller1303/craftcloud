//! Native HTTPS transport and local state adapters for CraftCloud apps.
use craftcloud_client::{
    CloudClient, CloudError, CloudTransport, Request, Response, UploadContent, Version,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use ureq::{
    Agent,
    config::Config,
    tls::{RootCerts, TlsConfig},
};
use uuid::Uuid;
use zeroize::Zeroize;

mod oidc;
pub use oidc::{Browser, OidcClientDetails, SystemBrowser, login_oidc};

/// Longest wait for name resolution and for connecting (including the TLS handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest wait for the request head to be written once connected.
const SEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest wait for the response head of a content transfer. The server answers an
/// upload only after it has read, verified and stored the content, so this is generous.
const TRANSFER_HEAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Total deadline of every request that is not a content transfer (login, refresh,
/// listings, commits). A token refresh holds the refresh locks for this long at most.
const SHORT_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest wait for the response head of such a request.
const SHORT_HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Slowest sustained transfer rate a body transfer must keep up (bytes per second).
const MIN_BODY_RATE: u64 = 32 * 1024;
/// Fixed allowance added to the size-dependent body budget.
const BODY_BASE_TIMEOUT: Duration = Duration::from_secs(30);

/// Time a body of `bytes` may take: a fixed allowance plus the time the slowest
/// accepted rate needs. The budget grows with the transfer, so large files on a slow
/// uplink succeed while a stalled connection still ends.
fn body_budget(bytes: u64) -> Duration {
    // Cap at one day so that an absurd size cannot overflow a Duration.
    BODY_BASE_TIMEOUT + Duration::from_secs((bytes / MIN_BODY_RATE).min(24 * 3600))
}

/// Content uploads and downloads (`.../content`) are the only requests with large bodies.
/// Chunk PUT routes carry content too and share the same size-dependent deadlines.
fn is_content_transfer(path: &str) -> bool {
    // SDK requests carry absolute URLs; transport tests also use relative paths.
    let Ok(uri) = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .parse::<ureq::http::Uri>()
    else {
        return false;
    };
    let path = uri.path();
    if path.ends_with("/content") {
        return true;
    }
    let Some((session, ordinal)) = path
        .strip_prefix("/api/v1/uploads/")
        .and_then(|tail| tail.split_once("/chunks/"))
    else {
        return false;
    };
    Uuid::parse_str(session).is_ok() && ordinal.parse::<u32>().is_ok() && !ordinal.contains('/')
}

/// How long the response body may take once its head arrived. A declared
/// `Content-Length` sets the budget (it has to fit `max_response_bytes`, otherwise the
/// body is refused); without it only a download may use the full allowance, anything else
/// gets the short fixed one.
fn response_budget(
    method: &str,
    path: &str,
    content_length: Option<u64>,
    max_response_bytes: u64,
) -> Result<Duration, CloudError> {
    match content_length {
        Some(length) if length > max_response_bytes => Err(CloudError::InvalidResponse),
        Some(length) if is_content_transfer(path) => Ok(body_budget(length)),
        Some(_) => Ok(BODY_BASE_TIMEOUT),
        None if method.eq_ignore_ascii_case("GET") && is_content_transfer(path) => {
            Ok(body_budget(max_response_bytes))
        }
        None => Ok(BODY_BASE_TIMEOUT),
    }
}

/// Separates failures that provably happened while connecting, before any request byte
/// left this host (nothing can have changed on the server), from failures with an unknown
/// outcome. Other I/O errors, even "unreachable" ones, may occur after the request was sent.
fn classify_ureq_error(error: &ureq::Error) -> CloudError {
    match error {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => CloudError::Unreachable,
        ureq::Error::Timeout(ureq::Timeout::Resolve | ureq::Timeout::Connect) => {
            CloudError::Unreachable
        }
        _ => CloudError::Offline,
    }
}

/// Reads `reader` to its end on a helper thread and gives up after `budget`, so a stalled
/// peer cannot keep the caller (and the locks it holds) beyond the budget. The helper
/// ends when its socket read fails or the transport's own backstop timeout fires.
fn read_with_deadline<R: Read + Send + 'static>(
    mut reader: R,
    max_bytes: usize,
    budget: Duration,
    cancelled: Option<&Arc<AtomicBool>>,
) -> Result<Vec<u8>, CloudError> {
    let abort = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&abort);
    let (done, result) = mpsc::channel();
    thread::Builder::new()
        .name("craftcloud-body-reader".into())
        .spawn(move || {
            let mut body = Vec::new();
            let mut chunk = [0_u8; 8192];
            let outcome = loop {
                if flag.load(Ordering::Acquire) {
                    break Err(CloudError::Cancelled);
                }
                match reader.read(&mut chunk) {
                    Ok(0) => break Ok(body),
                    Ok(count) => {
                        if body.len().saturating_add(count) > max_bytes {
                            break Err(CloudError::InvalidResponse);
                        }
                        match chunk.get(..count) {
                            Some(part) => body.extend_from_slice(part),
                            None => break Err(CloudError::InvalidResponse),
                        }
                    }
                    Err(error) => break Err(CloudError::Io(error.to_string())),
                }
            };
            let _ = done.send(outcome);
        })
        .map_err(|error| CloudError::Io(error.to_string()))?;
    let deadline = std::time::Instant::now() + budget;
    loop {
        match result.recv_timeout(Duration::from_millis(50)) {
            Ok(outcome) => return outcome,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(CloudError::Io("body reader stopped".into()));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancelled.is_some_and(|c| c.load(Ordering::Acquire)) {
                    abort.store(true, Ordering::Release);
                    return Err(CloudError::Cancelled);
                }
                if std::time::Instant::now() >= deadline {
                    abort.store(true, Ordering::Release);
                    // Part of the body may have arrived; the outcome is unknown.
                    return Err(CloudError::Offline);
                }
            }
        }
    }
}

/// Blocking ureq transport. Redirects are disabled; response bodies are capped.
///
/// Timeouts: connecting and sending the request head have fixed limits. Requests that are
/// not content transfers (login, refresh, listings, commits) have a short total deadline of
/// 60 s. Content transfers have no fixed total deadline: the upload gets a budget that
/// scales with its size, the download one that scales with its `Content-Length` (see
/// [`body_budget`]). ureq 3.4 offers no per-read idle timeout, so this is the closest
/// bound that still ends a stalled transfer.
pub struct UreqTransport {
    agent: Agent,
    max_response_bytes: u64,
}
impl UreqTransport {
    pub fn new(max_response_bytes: u64) -> Self {
        let config = Config::builder()
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_resolve(Some(CONNECT_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_send_request(Some(SEND_REQUEST_TIMEOUT))
            .timeout_recv_response(Some(TRANSFER_HEAD_TIMEOUT))
            .build();
        Self {
            agent: config.into(),
            max_response_bytes: max_response_bytes.max(1),
        }
    }
}
impl Default for UreqTransport {
    fn default() -> Self {
        Self::new(128 * 1024 * 1024)
    }
}
impl CloudTransport for UreqTransport {
    fn send(&self, req: Request) -> Result<Response, CloudError> {
        if req
            .cancelled
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Acquire))
        {
            return Err(CloudError::Cancelled);
        }
        let mut request = ureq::http::Request::builder()
            .method(req.method.as_str())
            .uri(req.path.as_str())
            .body(req.body)
            .map_err(|_| CloudError::InvalidUrl)?;
        for (k, v) in &req.headers {
            let name = ureq::http::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| CloudError::InvalidResponse)?;
            let value = ureq::http::header::HeaderValue::from_str(v)
                .map_err(|_| CloudError::InvalidResponse)?;
            request.headers_mut().append(name, value);
        }
        let transfer = is_content_transfer(req.path.as_str());
        let request = if transfer {
            let send_budget = body_budget(u64::try_from(request.body().len()).unwrap_or(u64::MAX));
            // Backstop only: the body read below enforces the real budget and gives up
            // earlier, once the response head has told the length.
            let receive_backstop = body_budget(self.max_response_bytes);
            self.agent
                .configure_request(request)
                .timeout_send_body(Some(send_budget))
                .timeout_recv_body(Some(receive_backstop))
                .build()
        } else {
            self.agent
                .configure_request(request)
                .timeout_global(Some(SHORT_CALL_TIMEOUT))
                .timeout_recv_response(Some(SHORT_HEAD_TIMEOUT))
                .build()
        };
        let response = self
            .agent
            .run(request)
            .map_err(|error| classify_ureq_error(&error))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| {
                Ok((
                    k.to_string(),
                    v.to_str()
                        .map_err(|_| CloudError::InvalidResponse)?
                        .to_string(),
                ))
            })
            .collect::<Result<Vec<_>, CloudError>>()?;
        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<u64>().ok());
        let budget = response_budget(
            req.method.as_str(),
            req.path.as_str(),
            content_length,
            self.max_response_bytes,
        )?;
        let body = read_with_deadline(
            response.into_body().into_reader(),
            usize::try_from(self.max_response_bytes).unwrap_or(usize::MAX),
            budget,
            req.cancelled.as_ref(),
        )?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

/// OS keychain implementation. Any unavailable keyring is surfaced; callers should continue without persistence.
pub struct KeychainTokenStore {
    service: String,
}
impl KeychainTokenStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
    fn entry(&self, server: &str, account: &str, app: &str) -> Result<keyring::Entry, CloudError> {
        let user = format!("{}:{}", account, app);
        keyring::Entry::new(&self.service, &format!("{}:{}", hash(server), user))
            .map_err(|_| CloudError::Io("keychain unavailable".into()))
    }
    pub fn save(
        &self,
        server: &str,
        account: &str,
        app: &str,
        refresh: &str,
    ) -> Result<(), CloudError> {
        self.entry(server, account, app)?
            .set_password(refresh)
            .map_err(|_| CloudError::Io("keychain unavailable".into()))
    }
    fn load_string(
        &self,
        server: &str,
        account: &str,
        app: &str,
    ) -> Result<Option<String>, CloudError> {
        match self.entry(server, account, app)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(CloudError::Io("keychain unavailable".into())),
        }
    }
    pub fn load(
        &self,
        server: &str,
        account: &str,
        app: &str,
    ) -> Result<Option<craftcloud_client::Secret>, CloudError> {
        self.load_string(server, account, app)
            .map(|token| token.map(craftcloud_client::Secret::new))
    }
    pub fn load_secret(
        &self,
        server: &str,
        account: &str,
        app: &str,
    ) -> Result<Option<craftcloud_client::Secret>, CloudError> {
        self.load(server, account, app)
    }
    pub fn delete(&self, server: &str, account: &str, app: &str) -> Result<(), CloudError> {
        match self.entry(server, account, app)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(CloudError::Io("keychain unavailable".into())),
        }
    }
    /// Keychain failure is the documented sign-in-each-start fallback.
    pub fn load_or_none(
        &self,
        server: &str,
        account: &str,
        app: &str,
    ) -> Option<craftcloud_client::Secret> {
        self.load(server, account, app).ok().flatten()
    }
    /// Return false when the OS has no usable keychain; callers keep the session in memory only.
    pub fn save_if_available(&self, server: &str, account: &str, app: &str, refresh: &str) -> bool {
        self.save(server, account, app, refresh).is_ok()
    }
    pub fn bound(
        &self,
        server: impl Into<String>,
        account: impl Into<String>,
        app: impl Into<String>,
    ) -> BoundKeychainTokenStore {
        BoundKeychainTokenStore {
            store: Self::new(self.service.clone()),
            server: server.into(),
            account: account.into(),
            app: app.into(),
            lock_dir: None,
            lock_wait: REFRESH_LOCK_WAIT,
        }
    }
}
/// How long a refresh waits for the lock file held by another process.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(10);
/// Serializes refresh rotation of every client in this process.
static REFRESH_PROCESS_LOCK: Mutex<()> = Mutex::new(());

/// Releases the refresh lock when dropped (file first, then the process lock).
struct RefreshLockGuard {
    _file: Option<fs::File>,
    _process: std::sync::MutexGuard<'static, ()>,
}

pub struct BoundKeychainTokenStore {
    store: KeychainTokenStore,
    server: String,
    account: String,
    app: String,
    lock_dir: Option<PathBuf>,
    lock_wait: Duration,
}
impl BoundKeychainTokenStore {
    pub fn load(&self) -> Result<Option<craftcloud_client::Secret>, CloudError> {
        self.store.load(&self.server, &self.account, &self.app)
    }
    /// Directory (normally the application data directory) for the lock file that
    /// serializes token refreshes between processes sharing this keychain entry. Without
    /// it, only clients inside this process are serialized.
    #[must_use]
    pub fn with_lock_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.lock_dir = Some(dir.into());
        self
    }
    /// Opens the lock file and takes the exclusive lock, polling until `lock_wait` passed.
    fn lock_file(&self, dir: &Path) -> Result<fs::File, CloudError> {
        fs::create_dir_all(dir).map_err(|e| CloudError::Io(e.to_string()))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path(dir))
            .map_err(|e| CloudError::Io(e.to_string()))?;
        let deadline = std::time::Instant::now() + self.lock_wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(CloudError::Io("refresh lock busy".into()));
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(CloudError::Io(error.to_string()));
                }
            }
        }
    }
    fn lock_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!(
            "craftcloud-refresh-{}.lock",
            hash(&format!("{}:{}:{}", self.server, self.account, self.app))
        ))
    }
}
impl craftcloud_client::RefreshTokenStore for BoundKeychainTokenStore {
    fn store(&self, refresh: Option<&craftcloud_client::Secret>) -> Result<(), CloudError> {
        match refresh {
            Some(token) => self
                .store
                .save(&self.server, &self.account, &self.app, token.expose()),
            None => self.store.delete(&self.server, &self.account, &self.app),
        }
    }
    fn load(&self) -> Result<Option<craftcloud_client::Secret>, CloudError> {
        BoundKeychainTokenStore::load(self)
    }
    fn lock_exclusive(&self) -> Result<Box<dyn std::any::Any>, CloudError> {
        // A holder that panicked leaves the mutex poisoned; the data is `()`, so recover.
        let process = REFRESH_PROCESS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The file lock only adds protection against other processes. When it cannot be
        // had in time (another process holds it, directory unusable), continue with the
        // process lock: a refresh then risks the reuse case that the lock prevents, but a
        // blocked sign-in is worse.
        let file = self
            .lock_dir
            .as_deref()
            .and_then(|dir| self.lock_file(dir).ok());
        Ok(Box::new(RefreshLockGuard {
            _file: file,
            _process: process,
        }))
    }
}

/// Persisted local-to-cloud association. No credentials are stored here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloudLink {
    pub local_path: PathBuf,
    pub server: String,
    pub file_id: String,
    pub base_version: Option<String>,
    pub sha256: String,
}
#[derive(Serialize, Deserialize, Default)]
struct LinkFile {
    links: Vec<CloudLink>,
}
pub struct LinksStore {
    path: PathBuf,
}
static LINKS_PROCESS_LOCK: Mutex<()> = Mutex::new(());
impl LinksStore {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: app_data_dir.as_ref().join("craftcloud-links.json"),
        }
    }
    pub fn all(&self) -> Result<Vec<CloudLink>, CloudError> {
        let s = self.with_lock(|| self.read_unlocked())?;
        Ok(s.links)
    }
    pub fn get(&self, path: &Path) -> Result<Option<CloudLink>, CloudError> {
        Ok(self
            .with_lock(|| self.read_unlocked())?
            .links
            .into_iter()
            .find(|v| v.local_path == path))
    }
    pub fn upsert(&self, link: CloudLink) -> Result<(), CloudError> {
        self.with_lock(|| {
            let mut data = self.read_unlocked()?;
            if let Some(old) = data
                .links
                .iter_mut()
                .find(|v| v.local_path == link.local_path)
            {
                *old = link
            } else {
                data.links.push(link)
            }
            self.write_unlocked(&data)
        })
    }
    pub fn remove(&self, path: &Path) -> Result<(), CloudError> {
        self.with_lock(|| {
            let mut data = self.read_unlocked()?;
            data.links.retain(|v| v.local_path != path);
            self.write_unlocked(&data)
        })
    }
    fn with_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, CloudError>,
    ) -> Result<T, CloudError> {
        let _process_guard = LINKS_PROCESS_LOCK
            .lock()
            .map_err(|_| CloudError::Io("link store lock poisoned".into()))?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| CloudError::Io(e.to_string()))?;
        }
        let lock_path = self.path.with_extension("json.lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| CloudError::Io(e.to_string()))?;
        lock_file
            .lock()
            .map_err(|e| CloudError::Io(e.to_string()))?;
        operation()
    }
    fn read_unlocked(&self) -> Result<LinkFile, CloudError> {
        match fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|_| CloudError::InvalidResponse),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let backup = self.path.with_extension("json.bak");
                match fs::read(&backup) {
                    Ok(b) => {
                        let _ = fs::rename(backup, &self.path);
                        serde_json::from_slice(&b).map_err(|_| CloudError::InvalidResponse)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LinkFile::default()),
                    Err(e) => Err(CloudError::Io(e.to_string())),
                }
            }
            Err(e) => Err(CloudError::Io(e.to_string())),
        }
    }
    fn write_unlocked(&self, data: &LinkFile) -> Result<(), CloudError> {
        if let Some(p) = self.path.parent() {
            fs::create_dir_all(p).map_err(|e| CloudError::Io(e.to_string()))?;
        }
        let bytes = serde_json::to_vec_pretty(data).map_err(|_| CloudError::InvalidResponse)?;
        let (parent, name) = (
            self.path.parent().ok_or(CloudError::InvalidUrl)?,
            self.path
                .file_name()
                .ok_or(CloudError::InvalidUrl)?
                .to_string_lossy(),
        );
        let (temp, mut f) = unique_temp_file(parent, &name)?;
        let result = (|| {
            f.write_all(&bytes)
                .map_err(|e| CloudError::Io(e.to_string()))?;
            f.sync_all().map_err(|e| CloudError::Io(e.to_string()))?;
            fs::rename(&temp, &self.path).map_err(|e| CloudError::Io(e.to_string()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

/// Writes the complete local file first, then uploads and records the association.
/// Cloud failure leaves the local bytes intact and the prior link unchanged.
pub enum SaveOutcome {
    LocalOnly,
    Uploaded(Version),
}
/// Cancellation and byte-progress callback for a native upload.
pub struct UploadControl<F> {
    pub cancel: Arc<AtomicBool>,
    pub on_progress: F,
}
#[derive(Serialize)]
struct CreateFileRequest<'a> {
    name: &'a str,
    app: &'a str,
    format: &'a str,
}
/// Explicit destination and metadata for the first cloud save.
pub struct NewFile {
    pub project_id: Uuid,
    pub name: String,
    pub app: String,
    pub format: String,
    pub idempotency_key: String,
}
/// Explicit first save creates remote metadata, then writes local bytes and commits version one.
pub fn save_new_local_first<T: CloudTransport>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    target: &NewFile,
    bytes: Vec<u8>,
) -> Result<Version, CloudError> {
    save_new_local_first_with_cancel(
        client,
        links,
        local_path,
        server,
        target,
        bytes,
        Arc::new(AtomicBool::new(false)),
    )
}
pub fn save_new_local_first_with_cancel<T: CloudTransport>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    target: &NewFile,
    bytes: Vec<u8>,
    cancel: Arc<AtomicBool>,
) -> Result<Version, CloudError> {
    save_new_local_first_with_progress_and_cancel(
        client,
        links,
        local_path,
        server,
        target,
        bytes,
        UploadControl {
            cancel,
            on_progress: |_, _| {},
        },
    )
}

/// Like [`save_new_local_first_with_cancel`], reporting completed upload bytes.
pub fn save_new_local_first_with_progress_and_cancel<T, F>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    target: &NewFile,
    bytes: Vec<u8>,
    control: UploadControl<F>,
) -> Result<Version, CloudError>
where
    T: CloudTransport,
    F: FnMut(u64, u64),
{
    let UploadControl {
        cancel,
        on_progress,
    } = control;
    atomic_write(local_path, &bytes)?;
    if cancel.load(Ordering::Acquire) {
        return Err(CloudError::Cancelled);
    }
    let digest = hash_bytes_cancellable(&bytes, &cancel)?;
    let request = CreateFileRequest {
        name: &target.name,
        app: &target.app,
        format: &target.format,
    };
    let file: craftcloud_client::File = client.post(
        &format!("/api/v1/projects/{}/files", target.project_id),
        &request,
    )?;
    let mut link = CloudLink {
        local_path: local_path.to_path_buf(),
        server: server.into(),
        file_id: file.id.to_string(),
        base_version: None,
        sha256: digest,
    };
    links.upsert(link.clone())?;
    let version = client.upload_with_cancel_and_progress(
        &UploadContent {
            file_id: file.id,
            base_version: None,
            idempotency_key: target.idempotency_key.clone(),
            bytes,
        },
        cancel,
        on_progress,
    )?;
    link.base_version = Some(version.id.to_string());
    links.upsert(link)?;
    Ok(version)
}
pub fn save_local_first<T: CloudTransport>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    upload: UploadContent,
) -> Result<SaveOutcome, CloudError> {
    save_local_first_with_cancel(
        client,
        links,
        local_path,
        server,
        upload,
        Arc::new(AtomicBool::new(false)),
    )
}
pub fn save_local_first_with_cancel<T: CloudTransport>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    upload: UploadContent,
    cancel: Arc<AtomicBool>,
) -> Result<SaveOutcome, CloudError> {
    save_local_first_with_progress_and_cancel(
        client,
        links,
        local_path,
        server,
        upload,
        UploadControl {
            cancel,
            on_progress: |_, _| {},
        },
    )
}

/// Like [`save_local_first_with_cancel`], reporting completed upload bytes.
pub fn save_local_first_with_progress_and_cancel<T, F>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    upload: UploadContent,
    control: UploadControl<F>,
) -> Result<SaveOutcome, CloudError>
where
    T: CloudTransport,
    F: FnMut(u64, u64),
{
    save_linked(client, links, local_path, server, upload, None, control)
}
/// Like [`save_local_first_with_cancel`], but commits against `head` (the server's
/// current head version, `None` for a file without versions) instead of the base version
/// recorded in the link. This is the explicit "overwrite the other change" path. The
/// stored link keeps its previous base until the commit succeeded, so a failed or
/// cancelled attempt cannot make a later plain save skip the conflict check.
pub fn overwrite_local_first_with_cancel<T: CloudTransport>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    upload: UploadContent,
    head: Option<Uuid>,
    cancel: Arc<AtomicBool>,
) -> Result<SaveOutcome, CloudError> {
    save_linked(
        client,
        links,
        local_path,
        server,
        upload,
        Some(head),
        UploadControl {
            cancel,
            on_progress: |_, _| {},
        },
    )
}
fn save_linked<T: CloudTransport, F: FnMut(u64, u64)>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    mut upload: UploadContent,
    base_override: Option<Option<Uuid>>,
    control: UploadControl<F>,
) -> Result<SaveOutcome, CloudError> {
    let UploadControl {
        cancel,
        on_progress,
    } = control;
    atomic_write(local_path, &upload.bytes)?;
    let Some(mut link) = links.get(local_path)? else {
        return Ok(SaveOutcome::LocalOnly);
    };
    if link.server != server {
        return Err(CloudError::Validation(
            "local link belongs to another server".into(),
        ));
    }
    let file_id = Uuid::parse_str(&link.file_id).map_err(|_| CloudError::InvalidResponse)?;
    let base_version = match base_override {
        Some(head) => head,
        None => link
            .base_version
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| CloudError::InvalidResponse)?,
    };
    let digest = hash_bytes_cancellable(&upload.bytes, &cancel)?;
    upload.file_id = file_id;
    upload.base_version = base_version;
    let version = client.upload_with_cancel_and_progress(&upload, cancel, on_progress)?;
    link.base_version = Some(version.id.to_string());
    link.sha256 = digest;
    links.upsert(link)?;
    upload.bytes.zeroize();
    Ok(SaveOutcome::Uploaded(version))
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CloudError> {
    let parent = path.parent().ok_or(CloudError::InvalidUrl)?;
    let name = path
        .file_name()
        .ok_or(CloudError::InvalidUrl)?
        .to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| CloudError::Io(e.to_string()))?
        .as_nanos();
    let temp = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), nonce));
    let mut f = fs::File::create(&temp).map_err(|e| CloudError::Io(e.to_string()))?;
    f.write_all(bytes)
        .map_err(|e| CloudError::Io(e.to_string()))?;
    f.sync_all().map_err(|e| CloudError::Io(e.to_string()))?;
    fs::rename(&temp, path).map_err(|e| CloudError::Io(e.to_string()))?;
    Ok(())
}

/// Cache file extension for a file whose registered format is `format` (for example
/// `fig`). The extension must be a short alphanumeric word; anything else, such as the
/// tail of a display name like `Logo v2.1 final`, yields `bin`.
pub fn cache_extension(format: &str) -> String {
    let ext = format.trim().trim_start_matches('.');
    if ext.is_empty() || ext.len() > 16 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        "bin".to_owned()
    } else {
        ext.to_ascii_lowercase()
    }
}

/// Safe version cache: server key is a hash; IDs and extensions are strict allowlisted path segments.
pub struct VersionCache {
    root: PathBuf,
}
impl VersionCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn store(
        &self,
        server: &str,
        file_id: &str,
        version_id: &str,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, CloudError> {
        for id in [file_id, version_id] {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                return Err(CloudError::InvalidUrl);
            }
        }
        let ext = extension.trim_start_matches('.');
        if ext.is_empty() || ext.len() > 16 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(CloudError::InvalidUrl);
        }
        let path = self
            .root
            .join(hash(server))
            .join(file_id)
            .join(format!("{version_id}.{ext}"));
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).map_err(|e| CloudError::Io(e.to_string()))?
        }
        let parent = path.parent().ok_or(CloudError::InvalidUrl)?;
        let stem = path
            .file_name()
            .ok_or(CloudError::InvalidUrl)?
            .to_string_lossy();
        let (tmp, mut file) = unique_temp_file(parent, &stem)?;
        let written = (|| {
            file.write_all(bytes)
                .map_err(|e| CloudError::Io(e.to_string()))?;
            file.sync_all().map_err(|e| CloudError::Io(e.to_string()))?;
            Ok(())
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        match fs::hard_link(&tmp, &path) {
            Ok(()) => {
                fs::remove_file(&tmp).map_err(|e| CloudError::Io(e.to_string()))?;
                Ok(path)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                let existing = fs::read(&path).map_err(|e| CloudError::Io(e.to_string()))?;
                if existing == bytes {
                    Ok(path)
                } else {
                    Err(CloudError::Io(
                        "cached version path already contains different data".into(),
                    ))
                }
            }
            Err(error) => {
                let _ = fs::remove_file(tmp);
                Err(CloudError::Io(error.to_string()))
            }
        }
    }
}

/// Interval between lease refreshes.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
/// How long dropping waits for the lease to be released.
const RELEASE_WAIT: Duration = Duration::from_secs(3);

/// Keys of leases whose release has not finished yet, so that a reopen of the same file
/// can wait for it (see [`wait_for_release`]).
static RELEASING: (Mutex<Vec<String>>, Condvar) = (Mutex::new(Vec::new()), Condvar::new());

/// Waits until a lease started with [`FileLeaseHeartbeat::start_keyed`] under `key` has
/// been released, at most `timeout`. Returns false when the release is still pending.
/// Call this before acquiring the same file again so the old release cannot drop the
/// new lease.
pub fn wait_for_release(key: &str, timeout: Duration) -> bool {
    let (lock, changed) = &RELEASING;
    let guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (guard, _) = changed
        .wait_timeout_while(guard, timeout, |keys| keys.iter().any(|k| k == key))
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    !guard.iter().any(|k| k == key)
}

/// Keeps a file lease alive every 60 seconds until dropped. Drop attempts release.
///
/// The release is performed by the heartbeat thread itself, after its current heartbeat
/// call has returned, so it can never overtake an in-flight heartbeat (which would renew
/// the lease again right after the release). Dropping waits at most [`RELEASE_WAIT`] for
/// that; a call stuck in a slow request is left to finish alone, and the lease expiry
/// covers a release that never happens.
pub struct FileLeaseHeartbeat {
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
    finished: Option<mpsc::Receiver<()>>,
    key: Option<String>,
}
impl FileLeaseHeartbeat {
    pub fn start<F, R>(heartbeat: F, release: R) -> Self
    where
        F: FnMut() + Send + 'static,
        R: FnOnce() + Send + 'static,
    {
        Self::spawn(None, HEARTBEAT_INTERVAL, heartbeat, release)
    }
    /// Like [`start`](Self::start); `key` (for example the file id) lets
    /// [`wait_for_release`] wait for this lease's release.
    pub fn start_keyed<F, R>(key: impl Into<String>, heartbeat: F, release: R) -> Self
    where
        F: FnMut() + Send + 'static,
        R: FnOnce() + Send + 'static,
    {
        Self::spawn(Some(key.into()), HEARTBEAT_INTERVAL, heartbeat, release)
    }
    fn spawn<F, R>(key: Option<String>, interval: Duration, mut heartbeat: F, release: R) -> Self
    where
        F: FnMut() + Send + 'static,
        R: FnOnce() + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let flag = Arc::clone(&stop);
        let signal = Arc::clone(&wake);
        let release_key = key.clone();
        // The sender is dropped when the thread ends, which the receiver observes.
        let (done, finished) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            let _done = done;
            loop {
                let (lock, cv) = &*signal;
                let Ok(guard) = lock.lock() else { break };
                if flag.load(Ordering::Acquire) {
                    break;
                }
                let Ok((guard, _)) = cv.wait_timeout(guard, interval) else {
                    break;
                };
                drop(guard);
                if flag.load(Ordering::Acquire) {
                    break;
                }
                heartbeat()
            }
            release();
            if let Some(key) = release_key {
                let (lock, changed) = &RELEASING;
                lock.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|k| *k != key);
                changed.notify_all();
            }
        });
        Self {
            stop,
            wake,
            thread: Some(thread),
            finished: Some(finished),
            key,
        }
    }
}
impl Drop for FileLeaseHeartbeat {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            RELEASING
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(key.clone());
        }
        let (lock, cv) = &*self.wake;
        if let Ok(_guard) = lock.lock() {
            self.stop.store(true, Ordering::Release);
            cv.notify_all();
        } else {
            self.stop.store(true, Ordering::Release);
            cv.notify_all();
        }
        let ended = self.finished.take().is_none_or(|finished| {
            matches!(
                finished.recv_timeout(RELEASE_WAIT),
                Err(mpsc::RecvTimeoutError::Disconnected)
            )
        });
        if let Some(t) = self.thread.take()
            && ended
        {
            let _ = t.join();
        }
    }
}
fn hash(value: &str) -> String {
    let h = Sha256::digest(value.as_bytes());
    h.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash_bytes_cancellable(value: &[u8], cancel: &AtomicBool) -> Result<String, CloudError> {
    let mut digest = Sha256::new();
    for block in value.chunks(256 * 1024) {
        if cancel.load(Ordering::Acquire) {
            return Err(CloudError::Cancelled);
        }
        digest.update(block);
    }
    if cancel.load(Ordering::Acquire) {
        return Err(CloudError::Cancelled);
    }
    let digest = digest.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn unique_temp_file(parent: &Path, stem: &str) -> Result<(PathBuf, fs::File), CloudError> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), sequence));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(CloudError::Io(error.to_string())),
        }
    }
    Err(CloudError::Io(
        "could not allocate unique temporary file".into(),
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keychain_binding_implements_refresh_store() {
        fn assert_store<T: craftcloud_client::RefreshTokenStore>(_: &T) {}
        let bound = KeychainTokenStore::new("test").bound(
            "https://server",
            "person@example.invalid",
            "app",
        );
        assert_store(&bound);
    }
    #[test]
    fn cache_rejects_path_traversal() {
        let d = std::env::temp_dir().join(format!("cc-cache-{}", std::process::id()));
        let c = VersionCache::new(d);
        assert!(
            c.store("https://example", "../x", "b", "fig", b"x")
                .is_err()
        );
    }
    #[test]
    fn links_start_empty_and_roundtrip() {
        let d = std::env::temp_dir().join(format!("cc-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let s = LinksStore::new(&d);
        let link = CloudLink {
            local_path: PathBuf::from("x.fig"),
            server: "https://s".into(),
            file_id: "id".into(),
            base_version: Some("ver".into()),
            sha256: "abc".into(),
        };
        assert!(s.upsert(link.clone()).is_ok());
        assert_eq!(s.get(Path::new("x.fig")), Ok(Some(link)));
        let _ = fs::remove_dir_all(d);
    }
    #[test]
    fn links_recover_backup_when_primary_is_missing() {
        let d = std::env::temp_dir().join(format!("cc-links-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let store = LinksStore::new(&d);
        assert!(fs::create_dir_all(&d).is_ok());
        let link = CloudLink {
            local_path: PathBuf::from("saved.fig"),
            server: "https://server".into(),
            file_id: "file".into(),
            base_version: None,
            sha256: "digest".into(),
        };
        let backup = store.path.with_extension("json.bak");
        let encoded = serde_json::to_vec(&LinkFile {
            links: vec![link.clone()],
        });
        assert!(encoded.is_ok());
        if let Ok(encoded) = encoded {
            assert!(fs::write(&backup, encoded).is_ok());
        }
        assert_eq!(store.all().ok(), Some(vec![link]));
        assert!(store.path.exists());
        let _ = fs::remove_dir_all(d);
    }
    #[test]
    fn concurrent_link_writers_keep_every_update() {
        use std::sync::Barrier;
        let d = std::env::temp_dir().join(format!("cc-links-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let barrier = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();
        for index in 0..8 {
            let root = d.clone();
            let gate = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                let store = LinksStore::new(root);
                let link = CloudLink {
                    local_path: PathBuf::from(format!("file-{index}.fig")),
                    server: "https://server".into(),
                    file_id: format!("file-{index}"),
                    base_version: None,
                    sha256: index.to_string(),
                };
                gate.wait();
                store.upsert(link)
            }));
        }
        barrier.wait();
        for worker in workers {
            assert!(matches!(worker.join(), Ok(Ok(()))));
        }
        let all = LinksStore::new(&d).all();
        assert_eq!(all.map(|v| v.len()).ok(), Some(8));
        let _ = fs::remove_dir_all(d);
    }
    #[test]
    fn unlinked_save_is_local_only() {
        struct Count(Arc<AtomicBool>);
        impl CloudTransport for Count {
            fn send(&self, _: Request) -> Result<Response, CloudError> {
                self.0.store(true, Ordering::Release);
                Err(CloudError::Offline)
            }
        }
        let d = std::env::temp_dir().join(format!("cc-unlinked-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        assert!(fs::create_dir_all(&d).is_ok());
        let path = d.join("new.svg");
        let attempted = Arc::new(AtomicBool::new(false));
        let client = CloudClient::new("https://cloud.example", Count(Arc::clone(&attempted)));
        assert!(client.is_ok());
        if let Ok(client) = client {
            let upload = UploadContent {
                file_id: uuid::Uuid::nil(),
                base_version: None,
                idempotency_key: "no-cloud".into(),
                bytes: b"local".to_vec(),
            };
            let saved = save_local_first(
                &client,
                &LinksStore::new(&d),
                &path,
                "https://cloud.example",
                upload,
            );
            assert!(matches!(saved, Ok(SaveOutcome::LocalOnly)));
            assert!(!attempted.load(Ordering::Acquire));
            assert_eq!(fs::read(path).ok(), Some(b"local".to_vec()));
        }
        let _ = fs::remove_dir_all(d);
    }
    #[test]
    fn cache_never_overwrites_edited_version_path() {
        let d = std::env::temp_dir().join(format!("cc-cache-edit-{}", std::process::id()));
        let c = VersionCache::new(&d);
        let path = c.store("https://s", "aabb", "ccdd", "fig", b"cloud bytes");
        assert!(path.is_ok());
        if let Ok(path) = path {
            assert!(fs::write(&path, b"user edits").is_ok());
            assert!(
                c.store("https://s", "aabb", "ccdd", "fig", b"cloud bytes")
                    .is_err()
            );
            assert_eq!(fs::read(path).ok(), Some(b"user edits".to_vec()));
        }
        let _ = fs::remove_dir_all(d);
    }
    #[test]
    fn concurrent_cache_publication_never_replaces_existing_version() {
        use std::sync::Barrier;
        let d = std::env::temp_dir().join(format!("cc-cache-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let barrier = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();
        for index in 0..8_u8 {
            let root = d.clone();
            let gate = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                let cache = VersionCache::new(root);
                let bytes = vec![index; 1024];
                gate.wait();
                cache.store("https://server", "aabb", "ccdd", "fig", &bytes)
            }));
        }
        barrier.wait();
        let mut stored = 0;
        for worker in workers {
            assert!(worker.join().is_ok_and(|result| {
                if result.is_ok() {
                    stored += 1;
                }
                true
            }));
        }
        assert_eq!(stored, 1);
        let content = fs::read(d.join(hash("https://server")).join("aabb").join("ccdd.fig"));
        assert!(content.is_ok_and(|bytes| {
            bytes.len() == 1024
                && bytes
                    .first()
                    .is_some_and(|first| bytes.iter().all(|b| b == first))
        }));
        let _ = fs::remove_dir_all(d);
    }
    #[test]
    fn links_update_and_offline_save_preserves_local_data() {
        struct Offline;
        impl CloudTransport for Offline {
            fn send(&self, _: Request) -> Result<Response, CloudError> {
                Err(CloudError::Offline)
            }
        }
        let d = std::env::temp_dir().join(format!("cc-local-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        assert!(fs::create_dir_all(&d).is_ok());
        let path = d.join("art.pcraft");
        let links = LinksStore::new(&d);
        let client = CloudClient::new("https://cloud.example", Offline);
        assert!(client.is_ok());
        if let Ok(client) = client {
            assert!(
                client
                    .set_tokens(craftcloud_client::DeviceTokens {
                        access_token: craftcloud_client::Secret::new("a"),
                        refresh_token: craftcloud_client::Secret::new("r"),
                        expires_at: String::new()
                    })
                    .is_ok()
            );
            let request = UploadContent {
                file_id: uuid::Uuid::nil(),
                base_version: None,
                idempotency_key: "offline-key".into(),
                bytes: b"saved locally".to_vec(),
            };
            let link = CloudLink {
                local_path: path.clone(),
                server: "https://cloud.example".into(),
                file_id: uuid::Uuid::nil().to_string(),
                base_version: Some(uuid::Uuid::nil().to_string()),
                sha256: String::new(),
            };
            assert!(links.upsert(link).is_ok());
            assert!(
                save_local_first(&client, &links, &path, "https://cloud.example", request).is_err()
            );
            assert_eq!(fs::read(&path).ok(), Some(b"saved locally".to_vec()));
            assert_eq!(links.all().map(|items| items.len()).ok(), Some(1));
        }
        let _ = fs::remove_dir_all(d);
    }
    #[test]
    fn redirects_are_not_followed() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0");
        assert!(listener.is_ok());
        if let Ok(listener) = listener {
            let address = listener.local_addr();
            assert!(address.is_ok());
            if let Ok(address) = address {
                let server = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let mut request = [0_u8; 2048];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://example.invalid/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                });
                let request = Request {
                    method: "GET".into(),
                    path: format!("http://{address}/secure"),
                    headers: vec![("Authorization".into(), "Bearer never-forward".into())],
                    body: Vec::new(),
                    cancelled: None,
                };
                assert!(matches!(
                    UreqTransport::default().send(request),
                    Ok(Response { status: 302, .. })
                ));
                let _ = server.join();
            }
        }
    }

    #[test]
    fn body_transfers_have_a_size_scaled_budget_and_no_fixed_total_deadline() {
        assert_eq!(body_budget(0), BODY_BASE_TIMEOUT);
        // 100 MiB at the minimum rate needs about 53 minutes: far beyond the old 60 s.
        let hundred_mib = body_budget(100 * 1024 * 1024);
        assert!(
            hundred_mib > Duration::from_secs(50 * 60),
            "{hundred_mib:?}"
        );
        assert!(body_budget(u64::MAX) <= Duration::from_secs(24 * 3600 + 30));
        let timeouts = UreqTransport::default().agent.config().timeouts();
        assert_eq!(timeouts.global, None);
        assert_eq!(timeouts.per_call, None);
        assert_eq!(timeouts.connect, Some(CONNECT_TIMEOUT));
        assert_eq!(timeouts.recv_response, Some(TRANSFER_HEAD_TIMEOUT));
    }

    #[test]
    fn only_connect_phase_failures_are_classified_as_unreachable() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            classify_ureq_error(&ureq::Error::HostNotFound),
            CloudError::Unreachable
        );
        assert_eq!(
            classify_ureq_error(&ureq::Error::ConnectionFailed),
            CloudError::Unreachable
        );
        for phase in [ureq::Timeout::Connect, ureq::Timeout::Resolve] {
            assert_eq!(
                classify_ureq_error(&ureq::Error::Timeout(phase)),
                CloudError::Unreachable
            );
        }
        // Anything else may have happened after request bytes were sent.
        for error in [
            ureq::Error::Timeout(ureq::Timeout::RecvResponse),
            ureq::Error::Timeout(ureq::Timeout::SendBody),
            ureq::Error::Io(Error::from(ErrorKind::ConnectionReset)),
            ureq::Error::Io(Error::from(ErrorKind::HostUnreachable)),
            ureq::Error::Io(Error::from(ErrorKind::NetworkUnreachable)),
            ureq::Error::Io(Error::from(ErrorKind::ConnectionRefused)),
            ureq::Error::Io(Error::from(ErrorKind::UnexpectedEof)),
        ] {
            assert_eq!(classify_ureq_error(&error), CloudError::Offline);
        }
    }

    #[test]
    fn response_budget_follows_content_length_and_stays_short_elsewhere() {
        let max = 128 * 1024 * 1024;
        let download = "/api/v1/files/x/versions/y/content";
        // A declared length sets the budget of a download: 100 MiB is far beyond 60 s.
        let hundred = 100 * 1024 * 1024;
        assert!(
            response_budget("GET", download, Some(hundred), max)
                .is_ok_and(|b| b == body_budget(hundred))
        );
        // A length above the cap is refused before anything is read.
        assert_eq!(
            response_budget("GET", download, Some(max + 1), max),
            Err(CloudError::InvalidResponse)
        );
        // Without a length only a download gets the full allowance.
        assert_eq!(
            response_budget("GET", download, None, max),
            Ok(body_budget(max))
        );
        for (method, path, length) in [
            ("GET", "/api/v1/organizations", None),
            ("POST", "/api/v1/auth/device/refresh", None),
            ("POST", "/api/v1/auth/device/refresh", Some(500)),
            ("PUT", "/api/v1/uploads/u/content", None),
            ("GET", "/api/v1/info?x=/content", None),
        ] {
            assert_eq!(
                response_budget(method, path, length, max),
                Ok(BODY_BASE_TIMEOUT),
                "{method} {path}"
            );
        }
        assert!(is_content_transfer("/api/v1/uploads/u/content"));
        let chunk = "/api/v1/uploads/00000000-0000-0000-0000-000000000001/chunks/3";
        let four_mib = 4 * 1024 * 1024;
        assert_eq!(
            response_budget("PUT", chunk, Some(four_mib), max),
            Ok(Duration::from_secs(158))
        );
        let absolute_chunk = format!("https://cloud.example{chunk}?request=one");
        assert!(is_content_transfer(&absolute_chunk));
        assert_eq!(
            response_budget("PUT", &absolute_chunk, Some(four_mib), max),
            Ok(Duration::from_secs(158))
        );
        assert!(!is_content_transfer(
            "https://cloud.example/api/v1/info?next=/api/v1/uploads/00000000-0000-0000-0000-000000000001/chunks/3"
        ));
        assert_eq!(
            response_budget(
                "PUT",
                "/api/v1/uploads/not-a-uuid/chunks/3",
                Some(four_mib),
                max
            ),
            Ok(BODY_BASE_TIMEOUT)
        );
        assert_eq!(
            response_budget(
                "PUT",
                "/api/v1/uploads/00000000-0000-0000-0000-000000000001/chunks/3/extra",
                Some(four_mib),
                max
            ),
            Ok(BODY_BASE_TIMEOUT)
        );
        assert_eq!(
            response_budget(
                "PUT",
                "/api/v1/uploads/00000000-0000-0000-0000-000000000001/chunks/4294967296",
                Some(four_mib),
                max
            ),
            Ok(BODY_BASE_TIMEOUT)
        );
        assert!(!is_content_transfer("/api/v1/files/x/content/list"));
    }

    #[test]
    fn a_stalled_body_is_abandoned_after_its_budget() {
        struct Stall;
        impl Read for Stall {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                thread::sleep(Duration::from_secs(5));
                Ok(0)
            }
        }
        let started = std::time::Instant::now();
        let result = read_with_deadline(Stall, 1024, Duration::from_millis(300), None);
        assert_eq!(result, Err(CloudError::Offline));
        assert!(started.elapsed() < Duration::from_secs(2));
        // A prompt body is returned whole, and an oversized one is refused.
        assert_eq!(
            read_with_deadline(&b"hello"[..], 1024, Duration::from_secs(5), None),
            Ok(b"hello".to_vec())
        );
        assert_eq!(
            read_with_deadline(&b"hello"[..], 3, Duration::from_secs(5), None),
            Err(CloudError::InvalidResponse)
        );
        let cancel = Arc::new(AtomicBool::new(true));
        assert_eq!(
            read_with_deadline(Stall, 1024, Duration::from_secs(5), Some(&cancel)),
            Err(CloudError::Cancelled)
        );
    }

    #[test]
    fn a_refused_connection_is_an_uncertain_failure() {
        use std::net::TcpListener;
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port());
        assert!(port.is_ok());
        if let Ok(port) = port {
            let request = Request {
                method: "GET".into(),
                path: format!("http://127.0.0.1:{port}/"),
                headers: Vec::new(),
                body: Vec::new(),
                cancelled: None,
            };
            let result = UreqTransport::default().send(request).map(|_| ());
            assert!(
                matches!(result, Err(CloudError::Offline | CloudError::Unreachable)),
                "{result:?}"
            );
        }
    }

    #[test]
    fn cache_extension_comes_from_the_format_and_falls_back_to_bin() {
        assert_eq!(cache_extension("fig"), "fig");
        assert_eq!(cache_extension(".PCRAFT"), "pcraft");
        assert_eq!(cache_extension("1 final"), "bin");
        assert_eq!(cache_extension("../x"), "bin");
        assert_eq!(cache_extension(""), "bin");
        assert_eq!(cache_extension("averyveryverylongextension"), "bin");
        // A name such as "Logo v2.1 final" must open from the cache.
        let d = std::env::temp_dir().join(format!("cc-cache-ext-{}", std::process::id()));
        let stored = VersionCache::new(&d).store(
            "https://s",
            "aabb",
            "ccdd",
            &cache_extension("Logo v2.1 final"),
            b"x",
        );
        assert!(stored.is_ok_and(|path| path.extension().is_some_and(|e| e == "bin")));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn refresh_lock_is_exclusive_across_handles_and_released_on_drop() {
        use craftcloud_client::RefreshTokenStore;
        let d = std::env::temp_dir().join(format!("cc-refresh-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let store = KeychainTokenStore::new("test").bound("https://s", "a@example.test", "app");
        let store = store.with_lock_dir(&d);
        let path = store.lock_path(&d);
        let guard = store.lock_exclusive();
        assert!(guard.is_ok());
        // Another handle on the lock file, standing for another process, is refused.
        let other = OpenOptions::new().read(true).write(true).open(&path);
        assert!(other.is_ok());
        if let Ok(other) = other {
            assert!(other.try_lock().is_err());
            drop(guard);
            assert!(other.try_lock().is_ok());
        }
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn release_runs_after_an_in_flight_heartbeat_and_dropping_does_not_wait_for_it() {
        let in_call = Arc::new(AtomicBool::new(false));
        let overlapped = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let (call_flag, overlap_flag, release_flag) = (
            Arc::clone(&in_call),
            Arc::clone(&overlapped),
            Arc::clone(&released),
        );
        let in_call_for_release = Arc::clone(&in_call);
        let lease = FileLeaseHeartbeat::spawn(
            None,
            Duration::from_millis(10),
            move || {
                call_flag.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(4500));
                call_flag.store(false, Ordering::SeqCst);
            },
            move || {
                if in_call_for_release.load(Ordering::SeqCst) {
                    overlap_flag.store(true, Ordering::SeqCst);
                }
                release_flag.store(true, Ordering::SeqCst);
            },
        );
        while !in_call.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(5));
        }
        let started = std::time::Instant::now();
        drop(lease);
        // Drop gave up after the release wait, long before the call ended.
        assert!(
            started.elapsed() < Duration::from_millis(4200),
            "{:?}",
            started.elapsed()
        );
        assert!(!released.load(Ordering::SeqCst));
        // The release follows the call, never overlaps it.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while !released.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(released.load(Ordering::SeqCst));
        assert!(!overlapped.load(Ordering::SeqCst));
    }

    #[test]
    fn a_reopen_can_wait_for_the_release_of_the_same_file() {
        let released = Arc::new(AtomicBool::new(false));
        let release_flag = Arc::clone(&released);
        let lease = FileLeaseHeartbeat::spawn(
            Some("file-wait-test".into()),
            Duration::from_millis(10),
            || {},
            move || {
                thread::sleep(Duration::from_millis(600));
                release_flag.store(true, Ordering::SeqCst);
            },
        );
        // Drop returns after the bounded wait or when the release is done.
        drop(lease);
        assert!(wait_for_release("file-wait-test", Duration::from_secs(5)));
        assert!(released.load(Ordering::SeqCst));
        // An unknown key never blocks.
        assert!(wait_for_release("never-started", Duration::from_millis(10)));

        // A release that outlasts the wait reports false.
        let slow = FileLeaseHeartbeat::spawn(
            Some("file-slow-test".into()),
            Duration::from_millis(10),
            || {},
            || thread::sleep(Duration::from_millis(5000)),
        );
        thread::sleep(Duration::from_millis(50));
        let handle = thread::spawn(move || drop(slow));
        thread::sleep(Duration::from_millis(100));
        assert!(!wait_for_release(
            "file-slow-test",
            Duration::from_millis(100)
        ));
        let _ = handle.join();
        assert!(wait_for_release("file-slow-test", Duration::from_secs(5)));
    }

    #[test]
    fn an_idle_lease_stops_promptly_and_releases_once() {
        let releases = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&releases);
        let lease = FileLeaseHeartbeat::start(
            || {},
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        );
        let started = std::time::Instant::now();
        drop(lease);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    struct Scripted {
        replies: Mutex<std::collections::VecDeque<Result<Response, CloudError>>>,
        seen: Mutex<Vec<Request>>,
    }
    impl CloudTransport for Scripted {
        fn send(&self, request: Request) -> Result<Response, CloudError> {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(request);
            }
            self.replies
                .lock()
                .map_err(|_| CloudError::Offline)?
                .pop_front()
                .unwrap_or(Err(CloudError::Offline))
        }
    }
    fn reply(status: u16, body: &str) -> Result<Response, CloudError> {
        Ok(Response {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        })
    }
    const SESSION: &str = r#"{"upload_id":"00000000-0000-0000-0000-000000000001","file_id":"00000000-0000-0000-0000-000000000002","state":"created","size_bytes":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}"#;
    const VERSION: &str = r#"{"id":"00000000-0000-0000-0000-0000000000aa","file_id":"00000000-0000-0000-0000-000000000002","number":2,"size_bytes":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}"#;

    #[test]
    fn native_linked_save_uses_chunked_upload_and_forwards_progress() {
        let file_id = Uuid::from_u128(2);
        let session_id = Uuid::from_u128(1);
        let transport = Scripted {
            replies: Mutex::new(vec![
                reply(
                    200,
                    r#"{"api_version":"1.0","feature_flags":{"chunked_upload":true}}"#,
                ),
                reply(201, &format!(
                    "{{\"upload_id\":\"{session_id}\",\"file_id\":\"{file_id}\",\"state\":\"created\",\"size_bytes\":3,\"sha256\":\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\",\"mode\":\"chunked\",\"missing_chunks\":[0]}}"
                )),
                reply(204, ""),
                reply(201, VERSION),
            ].into()),
            seen: Mutex::new(Vec::new()),
        };
        let client = CloudClient::new("https://cloud.example", transport);
        assert!(client.is_ok());
        let Some(client) = client.ok() else {
            return;
        };
        assert!(
            client
                .set_tokens(craftcloud_client::DeviceTokens {
                    access_token: craftcloud_client::Secret::new("a"),
                    refresh_token: craftcloud_client::Secret::new("r"),
                    expires_at: String::new(),
                })
                .is_ok()
        );
        let dir = std::env::temp_dir().join(format!("cc-native-progress-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        let path = dir.join("linked.fig");
        let links = LinksStore::new(&dir);
        assert!(
            links
                .upsert(CloudLink {
                    local_path: path.clone(),
                    server: "https://cloud.example".into(),
                    file_id: file_id.to_string(),
                    base_version: None,
                    sha256: String::new(),
                })
                .is_ok()
        );
        let upload = UploadContent {
            file_id: Uuid::nil(),
            base_version: None,
            idempotency_key: "native-progress".into(),
            bytes: b"abc".to_vec(),
        };
        let mut progress = Vec::new();
        let result = save_local_first_with_progress_and_cancel(
            &client,
            &links,
            &path,
            "https://cloud.example",
            upload,
            UploadControl {
                cancel: Arc::new(AtomicBool::new(false)),
                on_progress: |done, total| progress.push((done, total)),
            },
        );
        assert!(matches!(result, Ok(SaveOutcome::Uploaded(_))));
        assert_eq!(progress, vec![(0, 3), (3, 3)]);
        let requests = client.transport().seen.lock();
        assert!(requests.is_ok_and(|requests| {
            requests
                .iter()
                .any(|request| request.path.ends_with("/chunks/0"))
                && requests
                    .iter()
                    .any(|request| request.path.ends_with("/versions"))
        }));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn overwrite_uses_the_head_for_one_upload_and_moves_the_link_only_after_the_commit() {
        let old_base = uuid::Uuid::from_u128(1);
        let head = uuid::Uuid::from_u128(2);
        let d = std::env::temp_dir().join(format!("cc-overwrite-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        assert!(fs::create_dir_all(&d).is_ok());
        let path = d.join("art.pcraft");
        let links = LinksStore::new(&d);
        let link = CloudLink {
            local_path: path.clone(),
            server: "https://cloud.example".into(),
            file_id: uuid::Uuid::from_u128(2).to_string(),
            base_version: Some(old_base.to_string()),
            sha256: "old".into(),
        };
        assert!(links.upsert(link.clone()).is_ok());
        let upload = || UploadContent {
            file_id: uuid::Uuid::nil(),
            base_version: None,
            idempotency_key: "overwrite".into(),
            bytes: b"abc".to_vec(),
        };
        let signed_in = |replies: Vec<Result<Response, CloudError>>| {
            let client = CloudClient::new(
                "https://cloud.example",
                Scripted {
                    replies: Mutex::new(replies.into()),
                    seen: Mutex::new(Vec::new()),
                },
            );
            assert!(client.is_ok());
            client.ok().inspect(|client| {
                assert!(
                    client
                        .set_tokens(craftcloud_client::DeviceTokens {
                            access_token: craftcloud_client::Secret::new("a"),
                            refresh_token: craftcloud_client::Secret::new("r"),
                            expires_at: String::new(),
                        })
                        .is_ok()
                );
            })
        };

        // The commit fails: the stored base stays, so the next plain save still conflicts.
        if let Some(client) = signed_in(vec![
            reply(200, r#"{"api_version":"1.0","feature_flags":{}}"#),
            reply(201, SESSION),
            reply(204, ""),
            reply(503, "{}"),
        ]) {
            let failed = overwrite_local_first_with_cancel(
                &client,
                &links,
                &path,
                "https://cloud.example",
                upload(),
                Some(head),
                Arc::new(AtomicBool::new(false)),
            );
            assert!(failed.is_err());
            assert_eq!(links.get(&path).ok().flatten(), Some(link.clone()));
            let seen = client.transport().seen.lock().map(|v| v.clone());
            assert!(seen.as_ref().is_ok_and(|seen| {
                seen.last().is_some_and(|commit| {
                    commit
                        .headers
                        .iter()
                        .any(|(k, v)| k == "If-Match" && *v == format!("\"version:{head}\""))
                })
            }));
        }

        // The commit succeeds: only now the link moves to the new version.
        if let Some(client) = signed_in(vec![
            reply(200, r#"{"api_version":"1.0","feature_flags":{}}"#),
            reply(201, SESSION),
            reply(204, ""),
            reply(201, VERSION),
        ]) {
            let saved = overwrite_local_first_with_cancel(
                &client,
                &links,
                &path,
                "https://cloud.example",
                upload(),
                Some(head),
                Arc::new(AtomicBool::new(false)),
            );
            assert!(matches!(saved, Ok(SaveOutcome::Uploaded(_))));
            let stored = links.get(&path).ok().flatten();
            assert_eq!(
                stored.and_then(|link| link.base_version),
                Some(uuid::Uuid::from_u128(0xaa).to_string())
            );
        }
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn a_busy_lock_file_falls_back_to_the_process_lock_after_the_wait() {
        use craftcloud_client::RefreshTokenStore;
        let d = std::env::temp_dir().join(format!("cc-refresh-busy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let mut store = KeychainTokenStore::new("test")
            .bound("https://s", "busy@example.test", "app")
            .with_lock_dir(&d);
        store.lock_wait = Duration::from_millis(200);
        assert!(fs::create_dir_all(&d).is_ok());
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(store.lock_path(&d));
        assert!(holder.is_ok());
        if let Ok(holder) = holder {
            assert!(holder.lock().is_ok());
            let started = std::time::Instant::now();
            let guard = store.lock_exclusive();
            assert!(guard.is_ok(), "must not fail when the file lock is busy");
            assert!(started.elapsed() < Duration::from_secs(3));
            drop(guard);
        }
        // An unusable directory degrades the same way.
        let blocked = std::env::temp_dir().join(format!("cc-refresh-file-{}", std::process::id()));
        assert!(fs::write(&blocked, b"x").is_ok());
        let unusable = KeychainTokenStore::new("test")
            .bound("https://s", "busy@example.test", "app")
            .with_lock_dir(blocked.join("sub"));
        assert!(unusable.lock_exclusive().is_ok());
        let _ = fs::remove_file(blocked);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    #[allow(clippy::panic)] // Poisons the lock on purpose.
    fn a_poisoned_process_lock_is_recovered() {
        use craftcloud_client::RefreshTokenStore;
        let poisoner = thread::spawn(|| {
            let _guard = REFRESH_PROCESS_LOCK.lock();
            panic!("poison the refresh lock");
        });
        assert!(poisoner.join().is_err());
        let store = KeychainTokenStore::new("test").bound("https://s", "p@example.test", "app");
        assert!(store.lock_exclusive().is_ok());
    }
}
