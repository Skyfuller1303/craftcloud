//! Typed, I/O-free CraftCloud client. Network access is provided by a transport.
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroize;

pub mod dto;
pub use dto::*;

/// HTTP request passed to an implementation-specific transport.
#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub cancelled: Option<Arc<AtomicBool>>,
}
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("path", &self.path.split('?').next().unwrap_or("/"))
            .field("headers", &"[redacted]")
            .field("body", &format_args!("[{} bytes]", self.body.len()))
            .finish()
    }
}
/// HTTP response returned by a transport. Body is bounded by the transport implementation.
#[derive(Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &"[redacted]")
            .field("body", &format_args!("[{} bytes]", self.body.len()))
            .finish()
    }
}
impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Synchronous transport; callers with UI threads should invoke it on a worker.
pub trait CloudTransport: Send + Sync {
    fn send(&self, request: Request) -> Result<Response, CloudError>;
}

#[derive(Clone, PartialEq, Eq)]
pub enum CloudError {
    Offline,
    Cancelled,
    Unauthorized,
    SecondFactorChallenge(String),
    Forbidden,
    IncompatibleServer(String),
    RateLimited(Option<String>),
    QuotaExceeded,
    ChecksumMismatch,
    VersionConflict(Option<Uuid>),
    FileLocked(Option<String>),
    StorageUnavailable,
    NotFound,
    Validation(String),
    Server {
        status: u16,
        code: String,
        message: String,
    },
    InvalidResponse,
    InvalidUrl,
    Io(String),
}
impl fmt::Debug for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SecondFactorChallenge(_) => f.write_str("SecondFactorChallenge([redacted])"),
            Self::Server { status, code, .. } => f
                .debug_struct("Server")
                .field("status", status)
                .field("code", code)
                .finish(),
            other => write!(f, "{}", other),
        }
    }
}
impl fmt::Display for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline => f.write_str("CraftCloud is unreachable"),
            Self::Cancelled => f.write_str("Operation cancelled"),
            Self::Unauthorized => f.write_str("Sign in again"),
            Self::SecondFactorChallenge(_) => f.write_str("A second-factor code is required"),
            Self::Forbidden => f.write_str("Permission denied"),
            Self::IncompatibleServer(v) => write!(f, "Unsupported CraftCloud API version: {v}"),
            Self::RateLimited(_) => f.write_str("Too many requests"),
            Self::QuotaExceeded => f.write_str("Storage quota exceeded"),
            Self::ChecksumMismatch => f.write_str("Downloaded file checksum did not match"),
            Self::VersionConflict(_) => f.write_str("File changed on the server"),
            Self::FileLocked(_) => f.write_str("File is locked"),
            Self::StorageUnavailable => f.write_str("Server storage unavailable"),
            Self::NotFound => f.write_str("File not found"),
            Self::Validation(v) => write!(f, "Invalid request: {v}"),
            Self::Server { code, .. } => write!(f, "CraftCloud error: {code}"),
            Self::InvalidResponse => f.write_str("Invalid response from CraftCloud"),
            Self::InvalidUrl => f.write_str("Invalid CraftCloud URL"),
            Self::Io(v) => write!(f, "Local I/O error: {v}"),
        }
    }
}
impl std::error::Error for CloudError {}

/// Secret wrapper. Debug never exposes token contents; refresh rotation must not be retried after an uncertain response.
pub struct Secret(String);
impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}
#[derive(Debug)]
pub struct DeviceTokens {
    pub access_token: Secret,
    pub refresh_token: Secret,
    pub expires_at: String,
}

/// Host-owned persistence for refresh tokens only. Implementations must never log tokens.
/// Failure disables persistence for this session; the client attempts to remove stale data.
pub trait RefreshTokenStore: Send + Sync {
    fn store(&self, refresh: Option<&Secret>) -> Result<(), CloudError>;
}

/// Core session and API workflow. Refresh calls serialize within this client.
pub struct CloudClient<T: CloudTransport> {
    base: String,
    transport: T,
    tokens: Mutex<Option<DeviceTokens>>,
    user: Mutex<Option<DeviceUser>>,
    token_store: Mutex<Option<Arc<dyn RefreshTokenStore>>>,
    api_major: u32,
}
impl<T: CloudTransport> CloudClient<T> {
    pub fn new(base_url: impl Into<String>, transport: T) -> Result<Self, CloudError> {
        let raw = base_url.into();
        let parsed = url::Url::parse(&raw).map_err(|_| CloudError::InvalidUrl)?;
        let loopback = match parsed.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if parsed.username() != ""
            || parsed.password().is_some()
            || parsed.fragment().is_some()
            || parsed.query().is_some()
            || parsed.host().is_none()
            || !(parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback))
        {
            return Err(CloudError::InvalidUrl);
        }
        let base = parsed.as_str().trim_end_matches('/').to_string();
        Ok(Self {
            base,
            transport,
            tokens: Mutex::new(None),
            user: Mutex::new(None),
            token_store: Mutex::new(None),
            api_major: 1,
        })
    }
    pub fn transport(&self) -> &T {
        &self.transport
    }
    pub fn set_tokens(&self, tokens: DeviceTokens) -> Result<(), CloudError> {
        let mut slot = self.tokens.lock().map_err(|_| CloudError::Offline)?;
        self.persist_refresh(Some(&tokens.refresh_token));
        *slot = Some(tokens);
        Ok(())
    }
    /// Attach the host's account-scoped store before login or refresh restoration.
    pub fn set_refresh_token_store(
        &self,
        store: Arc<dyn RefreshTokenStore>,
    ) -> Result<(), CloudError> {
        *self.token_store.lock().map_err(|_| CloudError::Offline)? = Some(store);
        Ok(())
    }
    fn persist_refresh(&self, refresh: Option<&Secret>) {
        if let Ok(mut slot) = self.token_store.lock()
            && let Some(store) = slot.as_ref()
            && store.store(refresh).is_err()
        {
            let _ = store.store(None);
            *slot = None;
        }
    }
    pub fn clear_tokens(&self) {
        if let Ok(mut t) = self.tokens.lock() {
            *t = None;
            self.persist_refresh(None);
        }
        if let Ok(mut user) = self.user.lock() {
            *user = None;
        }
    }
    pub fn is_authenticated(&self) -> bool {
        self.tokens.lock().is_ok_and(|t| t.is_some())
    }
    pub fn refresh_token(&self) -> Result<Secret, CloudError> {
        self.tokens
            .lock()
            .map_err(|_| CloudError::Offline)?
            .as_ref()
            .map(|t| Secret::new(t.refresh_token.expose()))
            .ok_or(CloudError::Unauthorized)
    }
    pub fn restore_refresh_token(&self, token: Secret) {
        if let Ok(mut t) = self.tokens.lock() {
            *t = Some(DeviceTokens {
                access_token: Secret::new(""),
                refresh_token: token,
                expires_at: String::new(),
            });
        }
    }
    pub fn authenticated_user(&self) -> Option<DeviceUser> {
        self.user.lock().ok().and_then(|u| u.clone())
    }
    pub fn current_user(&self) -> Result<DeviceUser, CloudError> {
        let session: SessionInfo = self.get("/api/v1/auth/session")?;
        let user = DeviceUser {
            user_id: session.user_id,
            email: session.email,
            display_name: session.display_name,
            instance_role: session.instance_role,
        };
        if let Ok(mut slot) = self.user.lock() {
            *slot = Some(user.clone());
        }
        Ok(user)
    }
    pub fn discover(&self) -> Result<Discovery, CloudError> {
        let d: Discovery =
            self.json("GET", "/api/v1/info", None::<&serde_json::Value>, None, &[])?;
        if d.api_version.split('.').next() != Some(&self.api_major.to_string()) {
            return Err(CloudError::IncompatibleServer(d.api_version));
        }
        Ok(d)
    }
    pub fn login(
        &self,
        email: &str,
        password: &str,
        client_kind: &str,
        client_id: &str,
        device_name: &str,
    ) -> Result<(), CloudError> {
        self.clear_tokens();
        let body = serde_json::json!({"email":email,"password":password,"client_kind":client_kind,"client_id":client_id,"device_name":device_name});
        let t: DeviceLoginResponse =
            self.json("POST", "/api/v1/auth/device/login", Some(&body), None, &[])?;
        if let Ok(mut user) = self.user.lock() {
            *user = Some(t.user.clone());
        }
        self.set_tokens(t.into())
    }
    pub fn refresh(&self) -> Result<(), CloudError> {
        self.refresh_guarded(None)
    }
    fn refresh_if_current(&self, expected_access: &str) -> Result<(), CloudError> {
        self.refresh_guarded(Some(expected_access))
    }
    fn refresh_guarded(&self, expected: Option<&str>) -> Result<(), CloudError> {
        let mut g = self.tokens.lock().map_err(|_| CloudError::Offline)?;
        if let (Some(expected), Some(current)) = (expected, g.as_ref())
            && current.access_token.expose() != expected
        {
            return Ok(());
        }
        let old = g.take().ok_or(CloudError::Unauthorized)?;
        // Clear before rotation: a crash or lost response must not leave a used token persisted.
        self.persist_refresh(None);
        let body = serde_json::json!({"refresh_token":old.refresh_token.expose()});
        let result = self.json::<DeviceTokensDto, serde_json::Value>(
            "POST",
            "/api/v1/auth/device/refresh",
            Some(&body),
            None,
            &[],
        );
        match result {
            Ok(t) => {
                let tokens: DeviceTokens = t.into();
                self.persist_refresh(Some(&tokens.refresh_token));
                *g = Some(tokens);
                Ok(())
            }
            Err(e) => {
                *g = None;
                if let Ok(mut user) = self.user.lock() {
                    *user = None;
                }
                Err(e)
            }
        }
    }
    pub fn logout(&self) -> Result<(), CloudError> {
        let token = self.access_token()?;
        let result = self.raw(
            "POST",
            "/api/v1/auth/device/logout",
            Vec::new(),
            Some(&token),
            &[],
        );
        self.clear_tokens();
        result.map(|_| ())
    }
    pub fn get<TOut: DeserializeOwned>(&self, path: &str) -> Result<TOut, CloudError> {
        self.auth_json("GET", path, None::<&serde_json::Value>, None, &[])
    }
    pub fn post<TOut: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<TOut, CloudError> {
        self.auth_json("POST", path, Some(body), None, &[])
    }
    pub fn create_upload(
        &self,
        file_id: Uuid,
        bytes: &[u8],
        idempotency_key: &str,
    ) -> Result<PendingUpload, CloudError> {
        let body =
            serde_json::json!({"size_bytes":bytes.len(),"sha256":hex(&Sha256::digest(bytes))});
        let response: UploadSession = self.auth_json(
            "POST",
            &format!("/api/v1/files/{file_id}/uploads"),
            Some(&body),
            None,
            &[("Idempotency-Key".into(), idempotency_key.into())],
        )?;
        Ok(PendingUpload {
            upload_id: response.upload_id,
            file_id,
            idempotency_key: idempotency_key.into(),
        })
    }
    pub fn upload_content(
        &self,
        pending: &PendingUpload,
        bytes: Vec<u8>,
    ) -> Result<(), CloudError> {
        self.auth_bytes(
            "PUT",
            &format!("/api/v1/uploads/{}/content", pending.upload_id),
            bytes,
            &[("Content-Type".into(), "application/octet-stream".into())],
        )
        .map(|_| ())
    }
    pub fn commit_version(
        &self,
        pending: &PendingUpload,
        base_version: Option<Uuid>,
    ) -> Result<Version, CloudError> {
        let body = serde_json::json!({"upload_id":pending.upload_id});
        let headers = if let Some(base) = base_version {
            vec![
                ("If-Match".to_string(), format!("\"version:{base}\"")),
                ("Idempotency-Key".into(), pending.idempotency_key.clone()),
            ]
        } else {
            vec![
                ("If-None-Match".to_string(), "*".into()),
                ("Idempotency-Key".into(), pending.idempotency_key.clone()),
            ]
        };
        self.auth_json(
            "POST",
            &format!("/api/v1/files/{}/versions", pending.file_id),
            Some(&body),
            None,
            &headers,
        )
    }
    pub fn upload_with_cancel(
        &self,
        upload: &UploadContent,
        cancel: Arc<AtomicBool>,
    ) -> Result<Version, CloudError> {
        if cancel.load(Ordering::Acquire) {
            return Err(CloudError::Cancelled);
        }
        let digest = hex(&Sha256::digest(&upload.bytes));
        let key = upload.idempotency_key.clone();
        let create = serde_json::json!({"size_bytes":upload.bytes.len(),"sha256":digest});
        let session: UploadSession = self.auth_json(
            "POST",
            &format!("/api/v1/files/{}/uploads", upload.file_id),
            Some(&create),
            None,
            &[("Idempotency-Key".into(), key.clone())],
        )?;
        if cancel.load(Ordering::Acquire) {
            self.abort_upload(session.upload_id);
            return Err(CloudError::Cancelled);
        }
        let stored = self.cancellable_raw(
            "PUT",
            &format!("/api/v1/uploads/{}/content", session.upload_id),
            upload.bytes.clone(),
            vec![("Content-Type".into(), "application/octet-stream".into())],
            Arc::clone(&cancel),
        );
        if let Err(e) = stored {
            self.abort_upload(session.upload_id);
            return Err(e);
        }
        if cancel.load(Ordering::Acquire) {
            self.abort_upload(session.upload_id);
            return Err(CloudError::Cancelled);
        }
        let body = serde_json::json!({"upload_id":session.upload_id});
        let headers = if let Some(base) = upload.base_version {
            vec![
                ("If-Match".to_string(), format!("\"version:{base}\"")),
                ("Idempotency-Key".to_string(), key),
            ]
        } else {
            vec![
                ("If-None-Match".to_string(), "*".into()),
                ("Idempotency-Key".into(), key),
            ]
        };
        if cancel.load(Ordering::Acquire) {
            self.abort_upload(session.upload_id);
            return Err(CloudError::Cancelled);
        }
        self.auth_json(
            "POST",
            &format!("/api/v1/files/{}/versions", upload.file_id),
            Some(&body),
            None,
            &headers,
        )
    }
    pub fn upload(&self, upload: &UploadContent) -> Result<Version, CloudError> {
        self.upload_with_cancel(upload, Arc::new(AtomicBool::new(false)))
    }
    pub fn download(&self, file: Uuid, version: Option<Uuid>) -> Result<Vec<u8>, CloudError> {
        let path = version.map_or_else(
            || format!("/api/v1/files/{file}/content"),
            |v| format!("/api/v1/files/{file}/versions/{v}/content"),
        );
        let (bytes, response) = self.auth_bytes("GET", &path, Vec::new(), &[])?;
        let digest = response
            .header("Repr-Digest")
            .ok_or(CloudError::InvalidResponse)?;
        let expected = digest
            .strip_prefix("sha-256=:")
            .and_then(|v| v.strip_suffix(':'))
            .and_then(|v| STANDARD.decode(v).ok())
            .ok_or(CloudError::InvalidResponse)?;
        if expected.as_slice() != Sha256::digest(&bytes).as_slice() {
            return Err(CloudError::ChecksumMismatch);
        }
        Ok(bytes)
    }
    pub fn acquire_lock(&self, file: Uuid, client_id: &str) -> Result<LockInfo, CloudError> {
        self.auth_json(
            "POST",
            &format!("/api/v1/files/{file}/lock"),
            Some(&serde_json::json!({"client_id":client_id,"takeover":false})),
            None,
            &[],
        )
    }
    pub fn release_lock(&self, file: Uuid) -> Result<(), CloudError> {
        self.auth_bytes(
            "DELETE",
            &format!("/api/v1/files/{file}/lock"),
            Vec::new(),
            &[],
        )
        .map(|_| ())
    }
    fn auth_json<O: DeserializeOwned, B: Serialize>(
        &self,
        m: &str,
        p: &str,
        b: Option<&B>,
        token: Option<&Secret>,
        extra: &[(String, String)],
    ) -> Result<O, CloudError> {
        let owned = match token {
            Some(v) => Secret::new(v.expose()),
            None => self.access_token()?,
        };
        match self.json(m, p, b, Some(&owned), extra) {
            Err(CloudError::Unauthorized) if token.is_none() => {
                self.refresh_if_current(owned.expose())?;
                let fresh = self.access_token()?;
                self.json(m, p, b, Some(&fresh), extra)
            }
            v => v,
        }
    }
    fn cancellable_raw(
        &self,
        m: &str,
        p: &str,
        b: Vec<u8>,
        extra: Vec<(String, String)>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Response, CloudError> {
        if cancel.load(Ordering::Acquire) {
            return Err(CloudError::Cancelled);
        }
        let mut extra = extra;
        if !b.is_empty()
            && !extra
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            extra.push(("Content-Type".into(), "application/json".into()))
        }
        for attempt in 0..2 {
            if cancel.load(Ordering::Acquire) {
                return Err(CloudError::Cancelled);
            }
            let token = self.access_token()?;
            let mut headers = extra.clone();
            headers.push(("Authorization".into(), format!("Bearer {}", token.expose())));
            let response = self.transport.send(Request {
                method: m.into(),
                path: format!("{}{}", self.base, p),
                headers,
                body: b.clone(),
                cancelled: Some(Arc::clone(&cancel)),
            })?;
            if cancel.load(Ordering::Acquire) {
                return Err(CloudError::Cancelled);
            }
            if (200..300).contains(&response.status) {
                return Ok(response);
            }
            let error = error_for(response);
            if error == CloudError::Unauthorized && attempt == 0 {
                self.refresh_if_current(token.expose())?;
            } else {
                if error == CloudError::Unauthorized {
                    self.clear_tokens();
                }
                return Err(error);
            }
        }
        Err(CloudError::Unauthorized)
    }
    fn json<O: DeserializeOwned, B: Serialize>(
        &self,
        m: &str,
        p: &str,
        b: Option<&B>,
        token: Option<&Secret>,
        extra: &[(String, String)],
    ) -> Result<O, CloudError> {
        let body = b
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| CloudError::InvalidResponse)?
            .unwrap_or_default();
        let r = self.raw(m, p, body, token, extra)?;
        serde_json::from_slice(&r.body).map_err(|_| CloudError::InvalidResponse)
    }
    fn auth_bytes(
        &self,
        m: &str,
        p: &str,
        b: Vec<u8>,
        extra: &[(String, String)],
    ) -> Result<(Vec<u8>, Response), CloudError> {
        let token = self.access_token()?;
        match self.raw(m, p, b.clone(), Some(&token), extra) {
            Err(CloudError::Unauthorized) => {
                self.refresh_if_current(token.expose())?;
                let fresh = self.access_token()?;
                let r = self.raw(m, p, b, Some(&fresh), extra)?;
                Ok((r.body.clone(), r))
            }
            Ok(r) => Ok((r.body.clone(), r)),
            Err(e) => Err(e),
        }
    }
    fn abort_upload(&self, upload_id: Uuid) {
        let _ = self.auth_bytes(
            "DELETE",
            &format!("/api/v1/uploads/{upload_id}"),
            Vec::new(),
            &[],
        );
    }
    fn access_token(&self) -> Result<Secret, CloudError> {
        self.tokens
            .lock()
            .map_err(|_| CloudError::Offline)?
            .as_ref()
            .map(|t| Secret::new(t.access_token.expose()))
            .ok_or(CloudError::Unauthorized)
    }
    fn raw(
        &self,
        m: &str,
        p: &str,
        b: Vec<u8>,
        token: Option<&Secret>,
        extra: &[(String, String)],
    ) -> Result<Response, CloudError> {
        if !p.starts_with('/') || p.starts_with("//") {
            return Err(CloudError::InvalidUrl);
        }
        let mut h = extra.to_vec();
        if let Some(t) = token {
            h.push(("Authorization".into(), format!("Bearer {}", t.expose())))
        }
        if !b.is_empty()
            && !h
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            h.push(("Content-Type".into(), "application/json".into()))
        }
        let r = self.transport.send(Request {
            method: m.into(),
            path: format!("{}{}", self.base, p),
            headers: h,
            body: b,
            cancelled: None,
        })?;
        if (200..300).contains(&r.status) {
            Ok(r)
        } else {
            Err(error_for(r))
        }
    }
}

fn error_for(r: Response) -> CloudError {
    let e: Option<ErrorEnvelope> = serde_json::from_slice(&r.body).ok();
    let (code, message, details) = e
        .map(|e| (e.error.code, e.error.message, e.error.details))
        .unwrap_or_else(|| ("UNKNOWN".into(), String::new(), None));
    match (r.status, code.as_str()) {
        (401, "SECOND_FACTOR_REQUIRED") => CloudError::SecondFactorChallenge(
            details
                .as_ref()
                .and_then(|v| v.get("challenge_id").and_then(serde_json::Value::as_str))
                .unwrap_or_default()
                .to_string(),
        ),
        (401, _) => CloudError::Unauthorized,
        (403, _) => CloudError::Forbidden,
        (404, _) => CloudError::NotFound,
        (412, _) => CloudError::VersionConflict(
            details
                .as_ref()
                .and_then(|v| {
                    v.get("current_version_id")
                        .and_then(serde_json::Value::as_str)
                })
                .and_then(|s| Uuid::parse_str(s).ok()),
        ),
        (423, _) => CloudError::FileLocked(
            details
                .as_ref()
                .and_then(|v| {
                    v.get("holder")
                        .and_then(|h| h.get("display_name"))
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_string),
        ),
        (429, _) => CloudError::RateLimited(r.header("Retry-After").map(str::to_string)),
        (413 | 507, _) => CloudError::QuotaExceeded,
        (503, _) => CloudError::StorageUnavailable,
        (_, "CHECKSUM_MISMATCH") => CloudError::ChecksumMismatch,
        (_, _) => CloudError::Server {
            status: r.status,
            code,
            message,
        },
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}
#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
    details: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct UploadContent {
    pub file_id: Uuid,
    pub base_version: Option<Uuid>,
    pub idempotency_key: String,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone)]
pub struct PendingUpload {
    pub upload_id: Uuid,
    pub file_id: Uuid,
    pub idempotency_key: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct UploadSession {
    pub upload_id: Uuid,
    pub file_id: Uuid,
    pub size_bytes: u64,
    pub sha256: String,
    pub state: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Version {
    pub id: Uuid,
    pub file_id: Uuid,
    pub number: u64,
    pub size_bytes: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct LockInfo {
    pub holder_display_name: Option<String>,
    pub since: String,
    pub expires_at: String,
    pub mine: bool,
    pub held_by_current_session: bool,
}
#[derive(Deserialize)]
struct DeviceTokensDto {
    access_token: String,
    refresh_token: String,
    access_expires_at: String,
}
impl From<DeviceTokensDto> for DeviceTokens {
    fn from(v: DeviceTokensDto) -> Self {
        Self {
            access_token: Secret::new(v.access_token),
            refresh_token: Secret::new(v.refresh_token),
            expires_at: v.access_expires_at,
        }
    }
}
impl From<DeviceLoginResponse> for DeviceTokens {
    fn from(v: DeviceLoginResponse) -> Self {
        Self {
            access_token: Secret::new(v.access_token),
            refresh_token: Secret::new(v.refresh_token),
            expires_at: v.access_expires_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};
    fn response(status: u16, body: &str) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }
    struct Mock(Mutex<VecDeque<Result<Response, CloudError>>>);
    impl Mock {
        fn new(rs: Vec<Result<Response, CloudError>>) -> Self {
            Self(Mutex::new(rs.into()))
        }
    }
    impl CloudTransport for Mock {
        fn send(&self, _: Request) -> Result<Response, CloudError> {
            self.0
                .lock()
                .map_err(|_| CloudError::Offline)?
                .pop_front()
                .unwrap_or(Err(CloudError::Offline))
        }
    }
    #[test]
    fn token_debug_is_redacted() {
        let t = Secret::new("ccd_rt_test_secret");
        assert!(!format!("{t:?}").contains("test_secret"));
        assert_eq!(format!("{t:?}"), "***");
    }
    #[test]
    fn credential_containers_redact_debug() {
        let req = Request {
            method: "POST".into(),
            path: "/login".into(),
            headers: vec![("Authorization".into(), "Bearer secret".into())],
            body: b"password".to_vec(),
            cancelled: None,
        };
        let out = format!("{req:?}");
        assert!(!out.contains("secret") && !out.contains("password"));
        let response = Response {
            status: 200,
            headers: Vec::new(),
            body: b"ccd_rt_secret".to_vec(),
        };
        assert!(!format!("{response:?}").contains("ccd_rt_secret"));
    }
    #[test]
    fn only_https_or_explicit_loopback_is_accepted() {
        assert!(CloudClient::new("https://cloud.example", Mock::new(Vec::new())).is_ok());
        assert!(CloudClient::new("http://127.0.0.1:8080", Mock::new(Vec::new())).is_ok());
        assert!(CloudClient::new("http://[::1]:8080", Mock::new(Vec::new())).is_ok());
        assert!(CloudClient::new("http://localhost:8080", Mock::new(Vec::new())).is_ok());
        assert!(
            CloudClient::new("http://localhost:123@evil.example", Mock::new(Vec::new())).is_err()
        );
        assert!(CloudClient::new("http://cloud.example", Mock::new(Vec::new())).is_err());
    }
    #[test]
    fn maps_contract_error_details() {
        let r = response(
            401,
            r#"{"error":{"code":"SECOND_FACTOR_REQUIRED","message":"required","request_id":null,"details":{"challenge_id":"challenge"}}}"#,
        );
        assert!(matches!(error_for(r),CloudError::SecondFactorChallenge(v)if v=="challenge"));
    }
    #[test]
    fn dto_contract_matches_openapi_required_fields() -> Result<(), Box<dyn std::error::Error>> {
        let spec = include_str!("../../../contracts/openapi.yaml");
        let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(spec)?;
        let schemas = doc
            .get("components")
            .and_then(|v| v.get("schemas"))
            .ok_or("components.schemas missing")?;
        for (name, fields) in [
            (
                "DeviceLoginRequest",
                &[
                    "email",
                    "password",
                    "client_kind",
                    "client_id",
                    "device_name",
                ][..],
            ),
            (
                "DeviceTokenResponse",
                &[
                    "access_token",
                    "access_expires_at",
                    "refresh_token",
                    "refresh_expires_at",
                    "device_session_id",
                ][..],
            ),
            (
                "File",
                &[
                    "id",
                    "organization_id",
                    "workspace_id",
                    "project_id",
                    "head_version_id",
                    "sha256",
                ][..],
            ),
            (
                "Version",
                &["id", "file_id", "number", "size_bytes", "sha256"][..],
            ),
            (
                "Upload",
                &["upload_id", "file_id", "state", "size_bytes", "sha256"][..],
            ),
            ("LockAcquireRequest", &["client_id"][..]),
        ] {
            let required = schemas
                .get(name)
                .and_then(|v| v.get("required"))
                .and_then(serde_yaml_ng::Value::as_sequence)
                .ok_or("required list missing")?;
            for field in fields {
                assert!(
                    required.iter().any(|v| v.as_str() == Some(field)),
                    "{name}.{field} missing from OpenAPI required fields"
                );
            }
        }
        for expected in [
            "/auth/device/login:",
            "/auth/device/refresh:",
            "/files/{file_id}/uploads:",
            "/uploads/{upload_id}/content:",
            "Repr-Digest:",
            "CommitVersionRequest:",
        ] {
            assert!(
                spec.contains(expected),
                "OpenAPI contract missing {expected}"
            );
        }
        Ok(())
    }
    fn authenticated<T: CloudTransport>(mock: T) -> Result<CloudClient<T>, CloudError> {
        let c = CloudClient::new("https://cloud.example", mock)?;
        c.set_tokens(DeviceTokens {
            access_token: Secret::new("old-access"),
            refresh_token: Secret::new("old-refresh"),
            expires_at: String::new(),
        })?;
        Ok(c)
    }
    #[test]
    fn refresh_on_401_is_serialized_then_original_request_retried() {
        let m = Mock::new(vec![
            Ok(response(401, "{}")),
            Ok(response(
                200,
                r#"{"access_token":"new-a","access_expires_at":"later","refresh_token":"new-r","refresh_expires_at":"later","device_session_id":"00000000-0000-0000-0000-000000000001"}"#,
            )),
            Ok(response(200, r#"{"items":[],"next_cursor":null}"#)),
        ]);
        let c = authenticated(m);
        assert!(c.is_ok());
        if let Ok(c) = c {
            let page: Result<Page<String>, _> = c.get("/api/v1/organizations");
            assert!(page.is_ok());
        }
    }
    #[test]
    fn refresh_reuse_failure_clears_session() {
        let c = authenticated(Mock::new(vec![
            Ok(response(401, "{}")),
            Ok(response(
                401,
                r#"{"error":{"code":"REFRESH_TOKEN_INVALID","message":"invalid","request_id":null}}"#,
            )),
        ]));
        assert!(c.is_ok());
        if let Ok(c) = c {
            let result: Result<Page<String>, _> = c.get("/api/v1/organizations");
            assert!(matches!(result, Err(CloudError::Unauthorized)));
            assert!(!c.is_authenticated());
        }
    }
    #[test]
    fn logout_clears_session() {
        let c = authenticated(Mock::new(vec![Ok(response(204, ""))]));
        assert!(c.is_ok());
        if let Ok(c) = c {
            assert!(c.logout().is_ok());
            assert!(!c.is_authenticated());
        }
    }
    #[test]
    fn every_upload_stage_returns_recoverable_error() {
        let file = Uuid::nil();
        let input = UploadContent {
            file_id: file,
            base_version: None,
            idempotency_key: "upload-key".into(),
            bytes: b"abc".to_vec(),
        };
        let session = r#"{"upload_id":"00000000-0000-0000-0000-000000000001","file_id":"00000000-0000-0000-0000-000000000002","project_id":"00000000-0000-0000-0000-000000000003","workspace_id":"00000000-0000-0000-0000-000000000004","state":"created","size_bytes":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","max_bytes":3,"created_by":"00000000-0000-0000-0000-000000000005","created_at":"now","expires_at":"later"}"#;
        for responses in [
            vec![Ok(response(503, "{}"))],
            vec![Ok(response(201, session)), Ok(response(503, "{}"))],
            vec![
                Ok(response(201, session)),
                Ok(response(204, "")),
                Ok(response(
                    412,
                    r#"{"error":{"code":"VERSION_CONFLICT","message":"conflict","request_id":null,"details":{"current_version_id":"00000000-0000-0000-0000-000000000006"}}}"#,
                )),
            ],
        ] {
            let c = authenticated(Mock::new(responses));
            assert!(c.is_ok());
            if let Ok(c) = c {
                assert!(c.upload(&input).is_err());
            }
        }
    }
    #[test]
    fn locked_and_offline_are_recoverable() {
        let file = Uuid::nil();
        let locked = authenticated(Mock::new(vec![Ok(response(
            423,
            r#"{"error":{"code":"FILE_LOCKED","message":"locked","request_id":null,"details":{"holder":{"display_name":"Other"}}}}"#,
        ))]));
        assert!(locked.is_ok());
        if let Ok(locked) = locked {
            assert!(matches!(
                locked.acquire_lock(file, "client"),
                Err(CloudError::FileLocked(Some(_)))
            ));
        }
        let offline = authenticated(Mock::new(vec![Err(CloudError::Offline)]));
        assert!(offline.is_ok());
        if let Ok(offline) = offline {
            let page: Result<Page<String>, _> = offline.get("/api/v1/organizations");
            assert!(matches!(page, Err(CloudError::Offline)));
        }
    }
    #[test]
    fn cancellation_aborts_session_before_commit() {
        struct CancelTransport {
            calls: Mutex<Vec<(String, String)>>,
            flag: Arc<AtomicBool>,
        }
        impl CloudTransport for CancelTransport {
            fn send(&self, request: Request) -> Result<Response, CloudError> {
                if let Ok(mut calls) = self.calls.lock() {
                    calls.push((request.method.clone(), request.path.clone()));
                }
                if request.method == "PUT" {
                    self.flag.store(true, Ordering::Release);
                    return Ok(response(204, ""));
                }
                if request.method == "DELETE" {
                    return Ok(response(204, ""));
                }
                Ok(response(
                    201,
                    r#"{"upload_id":"00000000-0000-0000-0000-000000000001","file_id":"00000000-0000-0000-0000-000000000002","project_id":"00000000-0000-0000-0000-000000000003","workspace_id":"00000000-0000-0000-0000-000000000004","state":"created","size_bytes":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","max_bytes":3,"created_by":"00000000-0000-0000-0000-000000000005","created_at":"now","expires_at":"later"}"#,
                ))
            }
        }
        let flag = Arc::new(AtomicBool::new(false));
        let transport = CancelTransport {
            calls: Mutex::new(Vec::new()),
            flag: Arc::clone(&flag),
        };
        let client = authenticated(transport);
        assert!(client.is_ok());
        if let Ok(client) = client {
            let upload = UploadContent {
                file_id: Uuid::nil(),
                base_version: None,
                idempotency_key: "cancel-key".into(),
                bytes: b"abc".to_vec(),
            };
            assert!(matches!(
                client.upload_with_cancel(&upload, flag),
                Err(CloudError::Cancelled)
            ));
            let calls = client.transport().calls.lock();
            assert!(calls.is_ok());
            if let Ok(calls) = calls {
                assert!(calls.iter().any(|(method, path)| method == "DELETE"
                    && path.ends_with("/uploads/00000000-0000-0000-0000-000000000001")));
                assert!(!calls.iter().any(|(_, path)| path.ends_with("/versions")));
            }
        }
    }

    #[derive(Default)]
    struct RecordingStore(Mutex<Vec<Option<String>>>);
    impl RefreshTokenStore for RecordingStore {
        fn store(&self, token: Option<&Secret>) -> Result<(), CloudError> {
            self.0
                .lock()
                .map_err(|_| CloudError::Offline)?
                .push(token.map(|t| t.expose().to_owned()));
            Ok(())
        }
    }

    const ROTATED: &str = r#"{"access_token":"new-a","access_expires_at":"later","refresh_token":"new-r","refresh_expires_at":"later","device_session_id":"00000000-0000-0000-0000-000000000001"}"#;

    #[test]
    fn refresh_persists_rotation_and_removes_reused_or_lost_tokens() -> Result<(), CloudError> {
        let client = authenticated(Mock::new(vec![
            Ok(response(200, ROTATED)),
            Err(CloudError::Offline),
        ]))?;
        let store = Arc::new(RecordingStore::default());
        client.set_refresh_token_store(store.clone())?;
        client.refresh()?;
        assert!(client.is_authenticated());
        assert_eq!(client.refresh(), Err(CloudError::Offline));
        assert!(!client.is_authenticated());
        assert_eq!(
            *store.0.lock().map_err(|_| CloudError::Offline)?,
            vec![None, Some("new-r".into()), None]
        );
        // A lost response must never trigger an automatic retry with the old refresh token.
        assert_eq!(client.refresh(), Err(CloudError::Unauthorized));
        let reused = authenticated(Mock::new(vec![Ok(response(
            401,
            r#"{"error":{"code":"REFRESH_REUSED","message":"revoked"}}"#,
        ))]))?;
        reused.set_refresh_token_store(store.clone())?;
        assert_eq!(reused.refresh(), Err(CloudError::Unauthorized));
        assert!(!reused.is_authenticated());
        Ok(())
    }

    #[test]
    fn cancellable_put_refreshes_once_and_retries() -> Result<(), CloudError> {
        let client = authenticated(Mock::new(vec![
            Ok(response(401, "{}")),
            Ok(response(200, ROTATED)),
            Ok(response(204, "")),
        ]))?;
        let result = client.cancellable_raw(
            "PUT",
            "/api/v1/uploads/test/content",
            b"local bytes".to_vec(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        )?;
        assert_eq!(result.status, 204);
        assert_eq!(client.refresh_token()?.expose(), "new-r");
        Ok(())
    }

    #[test]
    fn simultaneous_unauthorized_requests_rotate_only_once() -> Result<(), CloudError> {
        struct ConcurrentTransport {
            ready: std::sync::Barrier,
            rotations: std::sync::atomic::AtomicUsize,
        }
        impl CloudTransport for ConcurrentTransport {
            fn send(&self, request: Request) -> Result<Response, CloudError> {
                if request.path.ends_with("/refresh") {
                    self.rotations.fetch_add(1, Ordering::SeqCst);
                    return Ok(response(200, ROTATED));
                }
                if request
                    .headers
                    .iter()
                    .any(|(key, value)| key == "Authorization" && value == "Bearer old-access")
                {
                    self.ready.wait();
                    return Ok(response(401, "{}"));
                }
                Ok(response(200, "{}"))
            }
        }
        let client = Arc::new(authenticated(ConcurrentTransport {
            ready: std::sync::Barrier::new(2),
            rotations: std::sync::atomic::AtomicUsize::new(0),
        })?);
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let client = client.clone();
                std::thread::spawn(move || client.get::<serde_json::Value>("/api/v1/organizations"))
            })
            .collect();
        for handle in handles {
            assert!(matches!(handle.join(), Ok(Ok(_))));
        }
        assert_eq!(client.transport().rotations.load(Ordering::SeqCst), 1);
        Ok(())
    }
}
