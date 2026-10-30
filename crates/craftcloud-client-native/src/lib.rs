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

/// Longest wait for name resolution and for connecting (including the TLS handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest wait for the request head to be written once connected.
const SEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for the response head after the request was sent. The server answers an
/// upload only after it has read, verified and stored the content, so this is generous.
const RESPONSE_HEAD_TIMEOUT: Duration = Duration::from_secs(120);
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

/// Separates failures that provably happened before any request byte left this host
/// (nothing can have changed on the server) from failures with an unknown outcome.
fn classify_ureq_error(error: &ureq::Error) -> CloudError {
    use std::io::ErrorKind;
    match error {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => CloudError::Unreachable,
        ureq::Error::Timeout(ureq::Timeout::Resolve | ureq::Timeout::Connect) => {
            CloudError::Unreachable
        }
        ureq::Error::Io(io)
            if matches!(
                io.kind(),
                ErrorKind::ConnectionRefused
                    | ErrorKind::HostUnreachable
                    | ErrorKind::NetworkUnreachable
                    | ErrorKind::AddrNotAvailable
            ) =>
        {
            CloudError::Unreachable
        }
        _ => CloudError::Offline,
    }
}

/// Blocking ureq transport. Redirects are disabled; response bodies are capped.
///
/// Timeouts: connecting, sending the request head and waiting for the response head have
/// fixed limits. Body transfers have no fixed total deadline; each gets a budget that
/// scales with its size (see [`body_budget`]). ureq 3.4 offers no per-read idle timeout,
/// so this is the closest bound that still ends a stalled transfer.
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
            .timeout_recv_response(Some(RESPONSE_HEAD_TIMEOUT))
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
        let send_budget = body_budget(u64::try_from(request.body().len()).unwrap_or(u64::MAX));
        let receive_budget = body_budget(self.max_response_bytes);
        let request = self
            .agent
            .configure_request(request)
            .timeout_send_body(Some(send_budget))
            .timeout_recv_body(Some(receive_budget))
            .build();
        let mut response = self
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
        let mut body = Vec::new();
        let mut reader = response.body_mut().as_reader();
        let mut chunk = [0_u8; 8192];
        loop {
            if req
                .cancelled
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Acquire))
            {
                return Err(CloudError::Cancelled);
            }
            let count = reader
                .read(&mut chunk)
                .map_err(|e| CloudError::Io(e.to_string()))?;
            if count == 0 {
                break;
            }
            if body.len().saturating_add(count)
                > usize::try_from(self.max_response_bytes).unwrap_or(usize::MAX)
            {
                return Err(CloudError::InvalidResponse);
            }
            let part = chunk.get(..count).ok_or(CloudError::InvalidResponse)?;
            body.extend_from_slice(part);
        }
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
        }
    }
}
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
        let process = REFRESH_PROCESS_LOCK
            .lock()
            .map_err(|_| CloudError::Io("refresh lock poisoned".into()))?;
        let file = match &self.lock_dir {
            Some(dir) => {
                fs::create_dir_all(dir).map_err(|e| CloudError::Io(e.to_string()))?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(self.lock_path(dir))
                    .map_err(|e| CloudError::Io(e.to_string()))?;
                file.lock().map_err(|e| CloudError::Io(e.to_string()))?;
                Some(file)
            }
            None => None,
        };
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
    atomic_write(local_path, &bytes)?;
    if cancel.load(Ordering::Acquire) {
        return Err(CloudError::Cancelled);
    }
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
        sha256: hash_bytes(&bytes),
    };
    links.upsert(link.clone())?;
    let version = client.upload_with_cancel(
        &UploadContent {
            file_id: file.id,
            base_version: None,
            idempotency_key: target.idempotency_key.clone(),
            bytes,
        },
        cancel,
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
    save_linked(client, links, local_path, server, upload, None, cancel)
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
        cancel,
    )
}
fn save_linked<T: CloudTransport>(
    client: &CloudClient<T>,
    links: &LinksStore,
    local_path: &Path,
    server: &str,
    mut upload: UploadContent,
    base_override: Option<Option<Uuid>>,
    cancel: Arc<AtomicBool>,
) -> Result<SaveOutcome, CloudError> {
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
    let digest = hash_bytes(&upload.bytes);
    upload.file_id = file_id;
    upload.base_version = base_version;
    let version = client.upload_with_cancel(&upload, cancel)?;
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
/// How long dropping waits for a heartbeat call that is still in flight.
const HEARTBEAT_STOP_GRACE: Duration = Duration::from_secs(1);
/// How long dropping waits for the release call before leaving it to finish alone.
const RELEASE_WAIT: Duration = Duration::from_secs(3);

/// Keeps a file lease alive every 60 seconds until dropped. Drop attempts release.
///
/// Dropping never waits for a network call longer than a few seconds: a heartbeat that
/// is stuck in a slow request is abandoned (it ends on its own and finds the stop flag
/// set), and the release runs on its own thread that is not waited for beyond
/// [`RELEASE_WAIT`]. A release that does not finish is covered by the lease expiry.
pub struct FileLeaseHeartbeat {
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
    finished: Option<mpsc::Receiver<()>>,
    release: Option<Box<dyn FnOnce() + Send>>,
}
impl FileLeaseHeartbeat {
    pub fn start<F, R>(heartbeat: F, release: R) -> Self
    where
        F: FnMut() + Send + 'static,
        R: FnOnce() + Send + 'static,
    {
        Self::start_with_interval(HEARTBEAT_INTERVAL, heartbeat, release)
    }
    fn start_with_interval<F, R>(interval: Duration, mut heartbeat: F, release: R) -> Self
    where
        F: FnMut() + Send + 'static,
        R: FnOnce() + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let flag = Arc::clone(&stop);
        let signal = Arc::clone(&wake);
        // The sender is dropped when the thread ends, which the receiver observes.
        let (done, finished) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            let _done = done;
            loop {
                let (lock, cv) = &*signal;
                let guard = match lock.lock() {
                    Ok(v) => v,
                    Err(_) => return,
                };
                if flag.load(Ordering::Acquire) {
                    return;
                }
                let (guard, _) = match cv.wait_timeout(guard, interval) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                drop(guard);
                if flag.load(Ordering::Acquire) {
                    return;
                }
                heartbeat()
            }
        });
        Self {
            stop,
            wake,
            thread: Some(thread),
            finished: Some(finished),
            release: Some(Box::new(release)),
        }
    }
}
impl Drop for FileLeaseHeartbeat {
    fn drop(&mut self) {
        let (lock, cv) = &*self.wake;
        if let Ok(_guard) = lock.lock() {
            self.stop.store(true, Ordering::Release);
            cv.notify_all();
        } else {
            self.stop.store(true, Ordering::Release);
            cv.notify_all();
        }
        // Join only a thread that has ended (or ends within the grace period); a thread
        // inside a slow request is detached instead of blocking the caller.
        let ended = self.finished.take().is_none_or(|finished| {
            matches!(
                finished.recv_timeout(HEARTBEAT_STOP_GRACE),
                Err(mpsc::RecvTimeoutError::Disconnected)
            )
        });
        if let Some(t) = self.thread.take()
            && ended
        {
            let _ = t.join();
        }
        if let Some(release) = self.release.take() {
            let (done, finished) = mpsc::channel::<()>();
            let spawned = thread::Builder::new()
                .name("craftcloud-lease-release".into())
                .spawn(move || {
                    release();
                    let _ = done.send(());
                });
            if spawned.is_ok() {
                let _ = finished.recv_timeout(RELEASE_WAIT);
            }
        }
    }
}
fn hash(value: &str) -> String {
    let h = Sha256::digest(value.as_bytes());
    h.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash_bytes(value: &[u8]) -> String {
    let h = Sha256::digest(value);
    h.iter().map(|b| format!("{b:02x}")).collect()
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
        assert_eq!(timeouts.recv_response, Some(RESPONSE_HEAD_TIMEOUT));
    }

    #[test]
    fn only_failures_before_the_request_left_are_classified_as_unreachable() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            classify_ureq_error(&ureq::Error::HostNotFound),
            CloudError::Unreachable
        );
        assert_eq!(
            classify_ureq_error(&ureq::Error::Timeout(ureq::Timeout::Connect)),
            CloudError::Unreachable
        );
        assert_eq!(
            classify_ureq_error(&ureq::Error::Io(Error::from(ErrorKind::ConnectionRefused))),
            CloudError::Unreachable
        );
        // Anything that can happen after request bytes were sent stays uncertain.
        for error in [
            ureq::Error::Timeout(ureq::Timeout::RecvResponse),
            ureq::Error::Timeout(ureq::Timeout::SendBody),
            ureq::Error::Io(Error::from(ErrorKind::ConnectionReset)),
            ureq::Error::Io(Error::from(ErrorKind::UnexpectedEof)),
        ] {
            assert_eq!(classify_ureq_error(&error), CloudError::Offline);
        }
    }

    #[test]
    fn a_refused_connection_is_reported_as_unreachable() {
        use std::net::TcpListener;
        // Bind and close a port so that nothing listens on it.
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
            assert_eq!(
                UreqTransport::default().send(request).map(|_| ()),
                Err(CloudError::Unreachable)
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
    fn dropping_a_lease_does_not_wait_for_slow_network_calls() {
        let released = Arc::new(AtomicBool::new(false));
        let release_flag = Arc::clone(&released);
        let in_call = Arc::new(AtomicBool::new(false));
        let call_flag = Arc::clone(&in_call);
        let lease = FileLeaseHeartbeat::start_with_interval(
            Duration::from_millis(10),
            move || {
                call_flag.store(true, Ordering::Release);
                thread::sleep(Duration::from_secs(8));
            },
            move || {
                release_flag.store(true, Ordering::Release);
                thread::sleep(Duration::from_secs(8));
            },
        );
        while !in_call.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }
        let started = std::time::Instant::now();
        drop(lease);
        let elapsed = started.elapsed();
        assert!(released.load(Ordering::Acquire));
        // Grace for the heartbeat plus the wait for the release, not the 16 s of calls.
        assert!(elapsed < Duration::from_secs(6), "{elapsed:?}");
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
        if let Some(client) = signed_in(vec![reply(201, SESSION), reply(204, ""), reply(503, "{}")])
        {
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
}
