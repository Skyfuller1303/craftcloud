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
    /// The outcome of the request is unknown: the connection broke or timed out
    /// after the request may have reached the server.
    Offline,
    /// The request provably never reached the server (name resolution failed,
    /// connection refused or timed out while connecting). Nothing was changed
    /// on the server, so a rotating request such as a token refresh is safe to
    /// repeat with the same credentials.
    Unreachable,
    Cancelled,
    Unauthorized,
    SecondFactorChallenge(String),
    Forbidden,
    AppNotAssigned,
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
            Self::Offline | Self::Unreachable => f.write_str("CraftCloud is unreachable"),
            Self::Cancelled => f.write_str("Operation cancelled"),
            Self::Unauthorized => f.write_str("Sign in again"),
            Self::SecondFactorChallenge(_) => f.write_str("A second-factor code is required"),
            Self::Forbidden => f.write_str("Permission denied"),
            Self::AppNotAssigned => {
                f.write_str("Dein Admin hat dir diese App für die Cloud nicht zugewiesen")
            }
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

/// Second-factor proof for [`CloudClient::complete_second_factor`]. The code is
/// held in a [`Secret`], so it is zeroized on drop and never shown by `Debug`.
pub enum Proof {
    /// Six-digit authenticator code.
    Totp(Secret),
    /// One-use recovery code.
    Recovery(Secret),
}
impl Proof {
    pub fn totp(code: impl Into<String>) -> Self {
        Self::Totp(Secret::new(code))
    }
    pub fn recovery(code: impl Into<String>) -> Self {
        Self::Recovery(Secret::new(code))
    }
}
impl fmt::Debug for Proof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Totp(_) => f.write_str("Proof::Totp(***)"),
            Self::Recovery(_) => f.write_str("Proof::Recovery(***)"),
        }
    }
}

/// Host-owned persistence for refresh tokens only. Implementations must never log tokens.
/// Failure disables persistence for this session; the client attempts to remove stale data.
///
/// Several clients or processes may share one persisted slot (same server, account and
/// application). Refresh tokens rotate and a reused token revokes the whole session family,
/// so a store shared that way should implement [`load`](Self::load) and
/// [`lock_exclusive`](Self::lock_exclusive): the client then re-reads the slot and holds the
/// lock for the whole read-rotate-store sequence.
pub trait RefreshTokenStore: Send + Sync {
    fn store(&self, refresh: Option<&Secret>) -> Result<(), CloudError>;
    /// Reads the currently persisted refresh token. The default reports that reading is
    /// not supported; the client then refreshes with its in-memory token.
    fn load(&self) -> Result<Option<Secret>, CloudError> {
        Err(CloudError::Io(
            "persisted refresh token cannot be read".into(),
        ))
    }
    /// Blocks until no other client or process using the same slot is refreshing and
    /// returns a guard that releases the lock when dropped. The default takes no lock.
    fn lock_exclusive(&self) -> Result<Box<dyn std::any::Any>, CloudError> {
        Ok(Box::new(()))
    }
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
        let response = self.raw("GET", "/api/v1/info", Vec::new(), None, &[])?;
        let value: serde_json::Value =
            serde_json::from_slice(&response.body).map_err(|_| CloudError::InvalidResponse)?;
        // The version decides compatibility before any other field is interpreted, so a
        // server of another major version is reported as such, not as a malformed answer.
        let version = value
            .get("api_version")
            .and_then(serde_json::Value::as_str)
            .ok_or(CloudError::InvalidResponse)?;
        if version.split('.').next() != Some(&self.api_major.to_string()) {
            return Err(CloudError::IncompatibleServer(version.to_owned()));
        }
        serde_json::from_value(value).map_err(|_| CloudError::InvalidResponse)
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
    /// Completes a login that answered [`CloudError::SecondFactorChallenge`] with the
    /// challenge id it carried. On success the client holds the session as after `login`.
    /// A wrong or already used proof answers [`CloudError::Unauthorized`]; the challenge
    /// allows only a few attempts, then the password step must be repeated.
    pub fn complete_second_factor(
        &self,
        challenge_id: &str,
        proof: Proof,
    ) -> Result<(), CloudError> {
        let (field, code) = match &proof {
            Proof::Totp(code) => ("code", code.expose()),
            Proof::Recovery(code) => ("recovery_code", code.expose()),
        };
        let mut body = serde_json::Map::new();
        body.insert("challenge_id".into(), challenge_id.into());
        body.insert(field.into(), code.into());
        let t: DeviceLoginResponse = self.json(
            "POST",
            "/api/v1/auth/device/login/second-factor",
            Some(&serde_json::Value::Object(body)),
            None,
            &[],
        )?;
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
        if g.is_none() {
            return Err(CloudError::Unauthorized);
        }
        let store = self.token_store.lock().ok().and_then(|slot| slot.clone());
        // Held until the rotated token is stored: no other client or process sharing the
        // persisted slot can rotate in between.
        let _exclusive = match store.as_ref() {
            Some(store) => Some(store.lock_exclusive()?),
            None => None,
        };
        let mut old = g.take().ok_or(CloudError::Unauthorized)?;
        if let Some(stored) = store.as_ref().and_then(|store| store.load().ok().flatten())
            && stored.expose() != old.refresh_token.expose()
        {
            // Another holder of the shared slot rotated the token. Ours is stale, and
            // presenting it would make the server revoke the whole session family.
            old.refresh_token = stored;
        }
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
            Err(CloudError::Unreachable) => {
                // The request never reached the server, so the token was not rotated.
                // Keep the session and restore the persisted copy for a later retry.
                self.persist_refresh(Some(&old.refresh_token));
                *g = Some(old);
                Err(CloudError::Unreachable)
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
    /// Ends the server-side device session (and with it the file locks it holds), then
    /// clears the local session in every case. An expired access token is refreshed once
    /// and the request repeated. When the server session still could not be ended, the
    /// local state is cleared nevertheless and the error is returned so that the caller
    /// can tell the user that only the local sign-out happened.
    pub fn logout(&self) -> Result<(), CloudError> {
        let token = self.access_token()?;
        let mut result = self.raw_logout(&token);
        if result == Err(CloudError::Unauthorized) {
            result = match self.refresh_if_current(token.expose()) {
                Ok(()) => self
                    .access_token()
                    .and_then(|fresh| self.raw_logout(&fresh)),
                // The server no longer accepts the refresh token either: the session is
                // already gone, which is what a logout wants.
                Err(CloudError::Unauthorized) => Ok(()),
                Err(error) => Err(error),
            };
        }
        self.clear_tokens();
        result
    }
    fn raw_logout(&self, token: &Secret) -> Result<(), CloudError> {
        self.raw(
            "POST",
            "/api/v1/auth/device/logout",
            Vec::new(),
            Some(token),
            &[],
        )
        .map(|_| ())
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
    /// Update a resource under its ETag without bypassing session refresh or URL checks.
    pub fn patch<TOut: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
        if_match: &str,
    ) -> Result<TOut, CloudError> {
        if if_match.is_empty() || if_match.contains(['\r', '\n']) {
            return Err(CloudError::Validation("invalid precondition".into()));
        }
        self.auth_json(
            "PATCH",
            path,
            Some(body),
            None,
            &[("If-Match".into(), if_match.into())],
        )
    }
    /// Delete a resource, accepting an empty successful response (including 204).
    pub fn delete(&self, path: &str) -> Result<(), CloudError> {
        self.auth_bytes("DELETE", path, Vec::new(), &[]).map(|_| ())
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
        (403, "APP_NOT_ASSIGNED") => CloudError::AppNotAssigned,
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
    #[test]
    fn app_not_assigned_has_specific_recoverable_error() {
        let error = error_for(response(
            403,
            r#"{"error":{"code":"APP_NOT_ASSIGNED","message":"not assigned","request_id":null}}"#,
        ));
        assert_eq!(error, CloudError::AppNotAssigned);
        assert_eq!(
            error.to_string(),
            "Dein Admin hat dir diese App für die Cloud nicht zugewiesen"
        );
        assert_eq!(error_for(response(403, "{}")), CloudError::Forbidden);
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
    fn conditional_patch_refreshes_and_delete_accepts_empty_response() -> Result<(), CloudError> {
        struct Recorded {
            replies: Mock,
            requests: Mutex<Vec<Request>>,
        }
        impl CloudTransport for Recorded {
            fn send(&self, request: Request) -> Result<Response, CloudError> {
                self.requests
                    .lock()
                    .map_err(|_| CloudError::Offline)?
                    .push(request);
                self.replies.send(Request {
                    method: String::new(),
                    path: String::new(),
                    headers: Vec::new(),
                    body: Vec::new(),
                    cancelled: None,
                })
            }
        }
        let client = authenticated(Recorded {
            replies: Mock::new(vec![
                Ok(response(401, "{}")),
                Ok(response(200, ROTATED)),
                Ok(response(200, r#"{"role":"viewer","version":3}"#)),
                Ok(response(204, "")),
            ]),
            requests: Mutex::new(Vec::new()),
        })?;
        let result: serde_json::Value = client.patch(
            "/api/v1/grants/test",
            &serde_json::json!({"role":"viewer"}),
            "\"grant-v2\"",
        )?;
        assert_eq!(
            result.get("version").and_then(serde_json::Value::as_u64),
            Some(3)
        );
        client.delete("/api/v1/grants/test")?;
        let calls = client
            .transport()
            .requests
            .lock()
            .map_err(|_| CloudError::Offline)?;
        assert_eq!(calls.len(), 4);
        for request in calls.iter().filter(|request| request.method == "PATCH") {
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(name, value)| name == "If-Match" && value == "\"grant-v2\"")
            );
            assert_eq!(request.path, "https://cloud.example/api/v1/grants/test");
        }
        let retried = calls.get(2).ok_or(CloudError::InvalidResponse)?;
        assert!(
            retried
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer new-a")
        );
        assert_eq!(
            calls.last().map(|request| request.method.as_str()),
            Some("DELETE")
        );
        Ok(())
    }

    #[test]
    fn mutations_reject_cross_origin_paths_and_header_injection() -> Result<(), CloudError> {
        let client = authenticated(Mock::new(Vec::new()))?;
        assert_eq!(
            client.delete("https://other.example/api/v1/grants/test"),
            Err(CloudError::InvalidUrl)
        );
        assert_eq!(
            client.delete("//other.example/api/v1/grants/test"),
            Err(CloudError::InvalidUrl)
        );
        let patch: Result<serde_json::Value, _> = client.patch(
            "/api/v1/grants/test",
            &serde_json::json!({"role":"viewer"}),
            "\"grant-v2\"\r\nAuthorization: other",
        );
        assert!(matches!(patch, Err(CloudError::Validation(_))));
        Ok(())
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

    /// Replays canned responses and records every request.
    struct Recording {
        responses: Mutex<VecDeque<Result<Response, CloudError>>>,
        requests: Mutex<Vec<(String, String, String)>>,
    }
    impl Recording {
        fn new(responses: Vec<Result<Response, CloudError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
        fn seen(&self) -> Vec<(String, String, String)> {
            self.requests.lock().map(|v| v.clone()).unwrap_or_default()
        }
    }
    impl CloudTransport for Recording {
        fn send(&self, request: Request) -> Result<Response, CloudError> {
            if let Ok(mut seen) = self.requests.lock() {
                seen.push((
                    request.method.clone(),
                    request.path.clone(),
                    String::from_utf8_lossy(&request.body).into_owned(),
                ));
            }
            self.responses
                .lock()
                .map_err(|_| CloudError::Offline)?
                .pop_front()
                .unwrap_or(Err(CloudError::Offline))
        }
    }

    const LOGIN: &str = r#"{"access_token":"login-a","access_expires_at":"later","refresh_token":"login-r","refresh_expires_at":"later","device_session_id":"00000000-0000-0000-0000-000000000001","user":{"user_id":"00000000-0000-0000-0000-000000000002","email":"a@example.test","display_name":"A","instance_role":null}}"#;

    #[test]
    fn second_factor_completion_posts_the_chosen_proof_and_stores_the_session()
    -> Result<(), CloudError> {
        for (proof, field) in [
            (Proof::totp("123456"), "code"),
            (Proof::recovery("ABCDE-FGHIJ"), "recovery_code"),
        ] {
            let client = CloudClient::new(
                "https://cloud.example",
                Recording::new(vec![Ok(response(200, LOGIN))]),
            )?;
            client.complete_second_factor("challenge-1", proof)?;
            assert!(client.is_authenticated());
            assert_eq!(client.refresh_token()?.expose(), "login-r");
            assert_eq!(
                client.authenticated_user().map(|user| user.display_name),
                Some("A".to_owned())
            );
            let seen = client.transport().seen();
            let (method, path, body) = seen.first().ok_or(CloudError::InvalidResponse)?;
            assert_eq!(method, "POST");
            assert_eq!(
                path,
                "https://cloud.example/api/v1/auth/device/login/second-factor"
            );
            let body: serde_json::Value =
                serde_json::from_str(body).map_err(|_| CloudError::InvalidResponse)?;
            assert_eq!(body["challenge_id"], "challenge-1");
            assert_eq!(body.as_object().map(|map| map.len()), Some(2));
            assert!(body.get(field).is_some());
        }
        Ok(())
    }

    #[test]
    fn rejected_second_factor_keeps_the_client_signed_out_and_proofs_are_redacted()
    -> Result<(), CloudError> {
        let client = CloudClient::new(
            "https://cloud.example",
            Recording::new(vec![Ok(response(
                401,
                r#"{"error":{"code":"INVALID_CREDENTIALS","message":"no"}}"#,
            ))]),
        )?;
        assert_eq!(
            client.complete_second_factor("challenge-1", Proof::totp("000000")),
            Err(CloudError::Unauthorized)
        );
        assert!(!client.is_authenticated());
        let shown = format!(
            "{:?} {:?}",
            Proof::totp("424242"),
            Proof::recovery("SECRET-CODE")
        );
        assert!(
            !shown.contains("424242") && !shown.contains("SECRET-CODE"),
            "{shown}"
        );
        Ok(())
    }

    /// One persisted slot shared by two clients, standing for two processes
    /// that use the same keyring entry.
    struct SharedSlot {
        value: Mutex<Option<String>>,
        busy: Arc<AtomicBool>,
        locks: std::sync::atomic::AtomicUsize,
    }
    impl SharedSlot {
        fn holding(token: &str) -> Arc<Self> {
            Arc::new(Self {
                value: Mutex::new(Some(token.into())),
                busy: Arc::new(AtomicBool::new(false)),
                locks: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    impl RefreshTokenStore for SharedSlot {
        fn store(&self, refresh: Option<&Secret>) -> Result<(), CloudError> {
            *self.value.lock().map_err(|_| CloudError::Offline)? =
                refresh.map(|token| token.expose().to_owned());
            Ok(())
        }
        fn load(&self) -> Result<Option<Secret>, CloudError> {
            Ok(self
                .value
                .lock()
                .map_err(|_| CloudError::Offline)?
                .clone()
                .map(Secret::new))
        }
        fn lock_exclusive(&self) -> Result<Box<dyn std::any::Any>, CloudError> {
            while self
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                std::thread::yield_now();
            }
            self.locks.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Release(Arc::clone(&self.busy))))
        }
    }

    fn rotated(access: &str, refresh: &str) -> Response {
        response(
            200,
            &format!(
                r#"{{"access_token":"{access}","access_expires_at":"later","refresh_token":"{refresh}","refresh_expires_at":"later","device_session_id":"00000000-0000-0000-0000-000000000001"}}"#
            ),
        )
    }

    #[test]
    fn a_client_adopts_the_token_another_holder_rotated_before_it_refreshes()
    -> Result<(), CloudError> {
        let slot = SharedSlot::holding("old-refresh");
        let first = authenticated(Recording::new(vec![
            Ok(response(401, "{}")),
            Ok(rotated("first-a", "first-r")),
            Ok(response(200, "{}")),
        ]))?;
        let second = authenticated(Recording::new(vec![
            Ok(response(401, "{}")),
            Ok(rotated("second-a", "second-r")),
            Ok(response(200, "{}")),
        ]))?;
        first.set_refresh_token_store(slot.clone())?;
        second.set_refresh_token_store(slot.clone())?;

        first.get::<serde_json::Value>("/api/v1/organizations")?;
        // The slot now holds "first-r"; the second client still remembers "old-refresh".
        second.get::<serde_json::Value>("/api/v1/organizations")?;

        let seen = second.transport().seen();
        let refresh = seen
            .iter()
            .find(|(_, path, _)| path.ends_with("/auth/device/refresh"))
            .ok_or(CloudError::InvalidResponse)?;
        assert!(refresh.2.contains("first-r"), "{}", refresh.2);
        assert!(!refresh.2.contains("old-refresh"), "{}", refresh.2);
        assert_eq!(second.refresh_token()?.expose(), "second-r");
        assert_eq!(
            slot.value
                .lock()
                .map_err(|_| CloudError::Offline)?
                .as_deref(),
            Some("second-r")
        );
        assert_eq!(slot.locks.load(Ordering::SeqCst), 2);
        assert!(!slot.busy.load(Ordering::SeqCst), "lock was not released");
        Ok(())
    }

    #[test]
    fn refresh_attempts_of_two_clients_never_overlap_on_one_slot() -> Result<(), CloudError> {
        struct Gate {
            inside: Arc<std::sync::atomic::AtomicUsize>,
            overlap: Arc<AtomicBool>,
        }
        impl CloudTransport for Gate {
            fn send(&self, request: Request) -> Result<Response, CloudError> {
                if request.path.ends_with("/refresh") {
                    if self.inside.fetch_add(1, Ordering::SeqCst) > 0 {
                        self.overlap.store(true, Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    self.inside.fetch_sub(1, Ordering::SeqCst);
                    return Ok(rotated("n-a", "n-r"));
                }
                Ok(response(200, "{}"))
            }
        }
        let slot = SharedSlot::holding("old-refresh");
        let inside = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let overlap = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let client = authenticated(Gate {
                inside: Arc::clone(&inside),
                overlap: Arc::clone(&overlap),
            })?;
            client.set_refresh_token_store(slot.clone())?;
            handles.push(std::thread::spawn(move || client.refresh()));
        }
        for handle in handles {
            assert!(matches!(handle.join(), Ok(Ok(()))));
        }
        assert!(!overlap.load(Ordering::SeqCst));
        Ok(())
    }

    #[test]
    fn logout_refreshes_an_expired_access_token_and_retries() -> Result<(), CloudError> {
        let client = authenticated(Recording::new(vec![
            Ok(response(401, "{}")),
            Ok(rotated("new-a", "new-r")),
            Ok(response(204, "")),
        ]))?;
        client.logout()?;
        assert!(!client.is_authenticated());
        let seen = client.transport().seen();
        assert_eq!(seen.len(), 3);
        assert!(
            seen.first()
                .is_some_and(|r| r.1.ends_with("/device/logout"))
        );
        assert!(
            seen.get(1)
                .is_some_and(|r| r.1.ends_with("/device/refresh"))
        );
        assert!(seen.get(2).is_some_and(|r| r.1.ends_with("/device/logout")));
        Ok(())
    }

    #[test]
    fn logout_that_cannot_end_the_server_session_clears_locally_and_reports_it()
    -> Result<(), CloudError> {
        // The retry after a successful refresh fails: session state is cleared anyway.
        let client = authenticated(Recording::new(vec![
            Ok(response(401, "{}")),
            Ok(rotated("new-a", "new-r")),
            Ok(response(503, "{}")),
        ]))?;
        assert_eq!(client.logout(), Err(CloudError::StorageUnavailable));
        assert!(!client.is_authenticated());
        // The server is unreachable: same contract.
        let offline = authenticated(Recording::new(vec![Err(CloudError::Unreachable)]))?;
        assert_eq!(offline.logout(), Err(CloudError::Unreachable));
        assert!(!offline.is_authenticated());
        Ok(())
    }

    #[test]
    fn a_refresh_that_never_reached_the_server_keeps_the_session() -> Result<(), CloudError> {
        let client = authenticated(Mock::new(vec![
            Err(CloudError::Unreachable),
            Ok(rotated("new-a", "new-r")),
        ]))?;
        let store = Arc::new(RecordingStore::default());
        client.set_refresh_token_store(store.clone())?;
        assert_eq!(client.refresh(), Err(CloudError::Unreachable));
        assert!(client.is_authenticated());
        assert_eq!(client.refresh_token()?.expose(), "old-refresh");
        // The persisted copy was restored after the temporary clear.
        assert_eq!(
            store.0.lock().map_err(|_| CloudError::Offline)?.last(),
            Some(&Some("old-refresh".to_owned()))
        );
        // A later attempt succeeds with the same, never rotated, token.
        client.refresh()?;
        assert_eq!(client.refresh_token()?.expose(), "new-r");
        Ok(())
    }

    #[test]
    fn an_uncertain_refresh_outcome_still_clears_the_session() -> Result<(), CloudError> {
        let client = authenticated(Mock::new(vec![Err(CloudError::Offline)]))?;
        assert_eq!(client.refresh(), Err(CloudError::Offline));
        assert!(!client.is_authenticated());
        Ok(())
    }

    #[test]
    fn discovery_reports_other_majors_and_accepts_older_servers() -> Result<(), CloudError> {
        let other = CloudClient::new(
            "https://cloud.example",
            Mock::new(vec![Ok(response(200, r#"{"api_version":"2.0"}"#))]),
        )?;
        assert_eq!(
            other.discover().map(|_| ()),
            Err(CloudError::IncompatibleServer("2.0".into()))
        );
        let minimal = CloudClient::new(
            "https://cloud.example",
            Mock::new(vec![Ok(response(200, r#"{"api_version":"1.0"}"#))]),
        )?;
        let discovery = minimal.discover()?;
        assert_eq!(discovery.api_version, "1.0");
        assert!(!discovery.feature_flags.two_factor);
        assert!(discovery.formats.is_empty());
        let broken = CloudClient::new(
            "https://cloud.example",
            Mock::new(vec![Ok(response(200, r#"{"server_version":"1"}"#))]),
        )?;
        assert_eq!(
            broken.discover().map(|_| ()),
            Err(CloudError::InvalidResponse)
        );
        Ok(())
    }
}
