use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use lettre::AsyncTransport;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{Delivery, MailMessage, MailTransport, SmtpConfig, SmtpTransport, TransportError};

const GRAPH_TOKEN: &str = "https://login.microsoftonline.com";
const GOOGLE_TOKEN: &str = "https://oauth2.googleapis.com/token";
const GRAPH_SEND: &str = "https://graph.microsoft.com/v1.0/users";
const MAX_SECRET_BYTES: u64 = 1_048_576;
const MAX_TOKEN_RESPONSE_BYTES: usize = 65_536;

#[derive(Clone)]
pub struct OAuthConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub credential: OAuthCredential,
}

#[derive(Clone)]
pub enum OAuthCredential {
    ClientSecretFile(String),
    CertificateAndKeyFiles {
        certificate_file: String,
        key_file: String,
    },
}

#[derive(Clone)]
pub struct GraphConfig {
    pub oauth: OAuthConfig,
    pub sender: String,
    pub save_to_sent: bool,
}

#[derive(Clone)]
pub enum OAuthProvider {
    Microsoft(OAuthConfig),
    Google {
        service_account_file: String,
        sender: String,
    },
}
#[derive(Clone)]
pub struct SmtpOAuthConfig {
    pub smtp: SmtpConfig,
    pub provider: OAuthProvider,
}

pub enum MailTransportConfig {
    Smtp(SmtpConfig),
    Graph(GraphConfig),
    SmtpOAuth(SmtpOAuthConfig),
}

pub enum ConfiguredTransport {
    Smtp(SmtpTransport, TransportStatus),
    Graph(GraphTransport),
    SmtpOAuth(SmtpOAuthTransport),
}

#[derive(Clone, Default)]
pub struct TransportStatus(Arc<RwLock<TransportStatusSnapshot>>);

#[derive(Debug, Clone, Default)]
pub struct TransportStatusSnapshot {
    pub active_transport: String,
    pub sender: String,
    pub certificate_expires_unix: Option<i64>,
    pub last_safe_error: Option<String>,
}

impl TransportStatus {
    pub fn snapshot(&self) -> TransportStatusSnapshot {
        self.0.read().map(|s| s.clone()).unwrap_or_default()
    }
    fn set_error(&self, code: &'static str) {
        if let Ok(mut s) = self.0.write() {
            s.last_safe_error = Some(code.to_owned());
        }
    }
    fn configure(&self, kind: &str, sender: &str) {
        if let Ok(mut s) = self.0.write() {
            s.active_transport = kind.into();
            s.sender = sender.into();
            s.last_safe_error = None;
        }
    }
    fn clear_error(&self) {
        if let Ok(mut s) = self.0.write() {
            s.last_safe_error = None;
        }
    }
    fn set_certificate_expiry(&self, expires: Option<i64>) {
        if let Ok(mut s) = self.0.write() {
            s.certificate_expires_unix = expires;
        }
    }
}

impl MailTransportConfig {
    pub fn build(self, status: TransportStatus) -> Result<ConfiguredTransport, TransportError> {
        status.set_certificate_expiry(None);
        match self {
            Self::Smtp(c) => {
                let sender = mailbox_address(&c.from)?;
                status.configure("smtp", &sender);
                Ok(ConfiguredTransport::Smtp(SmtpTransport::new(&c)?, status))
            }
            Self::Graph(c) => {
                preflight_oauth(&c.oauth, &status)?;
                status.configure("graph", &c.sender);
                Ok(ConfiguredTransport::Graph(GraphTransport::new(c, status)?))
            }
            Self::SmtpOAuth(c) => {
                preflight_smtp_oauth(&c, &status)?;
                let sender = mailbox_address(&c.smtp.from)?;
                status.configure("smtp", &sender);
                Ok(ConfiguredTransport::SmtpOAuth(SmtpOAuthTransport::new(
                    c, status,
                )?))
            }
        }
    }
}

impl MailTransport for ConfiguredTransport {
    async fn send(&self, m: MailMessage) -> Result<Delivery, TransportError> {
        let (result, status) = match self {
            Self::Smtp(transport, status) => (transport.send(m).await, status),
            Self::Graph(transport) => (transport.send(m).await, &transport.status),
            Self::SmtpOAuth(transport) => (transport.send(m).await, &transport.status),
        };
        match &result {
            Ok(_) => status.clear_error(),
            Err(error) => status.set_error(error.code()),
        }
        result
    }
}

#[derive(Clone)]
struct CachedToken {
    value: Zeroizing<String>,
    refresh_at: i64,
}

struct TokenProvider {
    client: reqwest::Client,
    kind: TokenKind,
    token: Mutex<Option<CachedToken>>,
    status: TransportStatus,
    token_endpoint: Option<String>,
}
enum TokenKind {
    Microsoft {
        config: OAuthConfig,
        scope: &'static str,
    },
    Google(GoogleConfig),
}
struct GoogleConfig {
    service_account_file: String,
    sender: String,
}

fn mailbox_address(value: &str) -> Result<String, TransportError> {
    let mailbox = value
        .parse::<lettre::message::Mailbox>()
        .map_err(|_| TransportError::Permanent)?;
    Ok(mailbox.email.to_string())
}
fn preflight_oauth(config: &OAuthConfig, status: &TransportStatus) -> Result<(), TransportError> {
    use x509_parser::prelude::FromDer;
    match &config.credential {
        OAuthCredential::ClientSecretFile(path) => {
            let secret = read_secret(path)?;
            if secret.is_empty() || std::str::from_utf8(&secret).is_err() {
                return Err(TransportError::Permanent);
            }
        }
        OAuthCredential::CertificateAndKeyFiles {
            certificate_file,
            key_file,
        } => {
            let cert_pem = read_secret(certificate_file)?;
            let _key = read_secret(key_file)?;
            let (_, pem) = x509_parser::pem::parse_x509_pem(&cert_pem)
                .map_err(|_| TransportError::Permanent)?;
            let (_, cert) = x509_parser::certificate::X509Certificate::from_der(&pem.contents)
                .map_err(|_| TransportError::Permanent)?;
            let now = OffsetDateTime::now_utc().unix_timestamp();
            let expiry = cert.validity().not_after.timestamp();
            if now < cert.validity().not_before.timestamp() || now >= expiry {
                return Err(TransportError::Permanent);
            }
            let endpoint = format!("{GRAPH_TOKEN}/{}/oauth2/v2.0/token", config.tenant_id);
            let _ = microsoft_certificate_assertion(config, certificate_file, key_file, &endpoint)?;
            status.set_certificate_expiry(Some(expiry));
        }
    }
    Ok(())
}
fn preflight_smtp_oauth(
    config: &SmtpOAuthConfig,
    status: &TransportStatus,
) -> Result<(), TransportError> {
    match &config.provider {
        OAuthProvider::Microsoft(oauth) => preflight_oauth(oauth, status),
        OAuthProvider::Google {
            service_account_file,
            sender,
        } => {
            let data = read_secret(service_account_file)?;
            let account: ServiceAccount =
                serde_json::from_slice(&data).map_err(|_| TransportError::Permanent)?;
            if !valid_bare_mailbox(sender) || !valid_bare_mailbox(&account.client_email) {
                return Err(TransportError::Permanent);
            }
            let key = Zeroizing::new(account.private_key.clone());
            EncodingKey::from_rsa_pem(key.as_bytes()).map_err(|_| TransportError::Permanent)?;
            Ok(())
        }
    }
}

impl TokenProvider {
    fn microsoft(
        config: OAuthConfig,
        scope: &'static str,
        status: TransportStatus,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            client: http_client(None)?,
            kind: TokenKind::Microsoft { config, scope },
            token: Mutex::new(None),
            status,
            token_endpoint: None,
        })
    }
    fn google(config: GoogleConfig, status: TransportStatus) -> Result<Self, TransportError> {
        Ok(Self {
            client: http_client(None)?,
            kind: TokenKind::Google(config),
            token: Mutex::new(None),
            status,
            token_endpoint: None,
        })
    }
    async fn token(&self, force: bool) -> Result<Zeroizing<String>, TransportError> {
        let mut guard = self.token.lock().await;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        if !force
            && let Some(t) = guard.as_ref()
            && t.refresh_at > now
        {
            return Ok(Zeroizing::new(t.value.to_string()));
        }
        let (url, mut form) = match &self.kind {
            TokenKind::Microsoft { config, scope } => microsoft_form(config, scope)?,
            TokenKind::Google(c) => google_form(c).await?,
        };
        let url = self.token_endpoint.as_deref().unwrap_or(&url);
        let sent = self.client.post(url).form(&form).send().await;
        for (_, value) in &mut form {
            zeroize::Zeroize::zeroize(value);
        }
        let response = sent.map_err(|_| {
            self.status.set_error("provider_unavailable");
            TransportError::Transient
        })?;
        let status = response.status();
        let retry_after = parse_retry_after(
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok()),
            now,
        );
        let body = match read_bounded(response, MAX_TOKEN_RESPONSE_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                self.status.set_error(error.code());
                return Err(error);
            }
        };
        let mut value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => {
                let error = provider_error(status.as_u16(), None, retry_after);
                self.status.set_error(error.code());
                return Err(error);
            }
        };
        if !status.is_success() {
            let code = token_provider_code(&value);
            let error = provider_error(status.as_u16(), code, retry_after);
            self.status.set_error(error.code());
            return Err(error);
        }
        if !matches!(value.get("token_type").and_then(Value::as_str), Some(kind) if kind.eq_ignore_ascii_case("bearer"))
        {
            self.status.set_error("provider_invalid_token_type");
            return Err(TransportError::Provider {
                permanent: true,
                code: "provider_invalid_token_type",
                retry_after: None,
            });
        }
        let expires = value
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|seconds| (1..=86_400).contains(seconds))
            .ok_or_else(|| {
                self.status.set_error("provider_invalid_expiry");
                TransportError::Provider {
                    permanent: true,
                    code: "provider_invalid_expiry",
                    retry_after: None,
                }
            })?;
        let refresh_at = now.saturating_add(expires.saturating_sub(300).max(0));
        let access = value
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && !s.bytes().any(|b| b < 0x21 || b == 0x7f))
            .ok_or_else(|| {
                self.status.set_error("provider_invalid_token");
                TransportError::Provider {
                    permanent: true,
                    code: "provider_invalid_token",
                    retry_after: None,
                }
            })?;
        let access = Zeroizing::new(access.to_owned());
        if let Some(Value::String(secret)) = value.get_mut("access_token") {
            zeroize::Zeroize::zeroize(secret);
        }
        *guard = Some(CachedToken {
            value: Zeroizing::new(access.to_string()),
            refresh_at,
        });
        self.status.clear_error();
        Ok(access)
    }
    async fn invalidate(&self) {
        *self.token.lock().await = None;
    }
}

fn http_client(root_ca: Option<&[u8]>) -> Result<reqwest::Client, TransportError> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(pem) = root_ca {
        builder = builder.add_root_certificate(
            reqwest::Certificate::from_pem(pem).map_err(|_| TransportError::Permanent)?,
        );
    }
    builder.build().map_err(|_| TransportError::Permanent)
}
#[cfg(feature = "test-util")]
fn validate_test_endpoint(value: &str) -> Result<(), TransportError> {
    let url = url::Url::parse(value).map_err(|_| TransportError::Permanent)?;
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
    {
        return Err(TransportError::Permanent);
    }
    Ok(())
}
async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Zeroizing<Vec<u8>>, TransportError> {
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| TransportError::Transient)?
    {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(TransportError::Provider {
                permanent: false,
                code: "provider_response_too_large",
                retry_after: None,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
fn parse_retry_after(value: Option<&str>, now: i64) -> Option<Duration> {
    let seconds = value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or_else(|| {
            let date = httpdate::parse_http_date(value?).ok()?;
            let now = std::time::UNIX_EPOCH.checked_add(Duration::from_secs(now.max(0) as u64))?;
            Some(date.duration_since(now).unwrap_or_default().as_secs())
        })?;
    Some(Duration::from_secs(seconds.min(86_400)))
}
fn token_provider_code(value: &Value) -> Option<&'static str> {
    match value.get("error").and_then(Value::as_str)? {
        "invalid_client" => Some("invalid_client"),
        "invalid_grant" => Some("invalid_grant"),
        "unauthorized_client" => Some("unauthorized_client"),
        "invalid_scope" => Some("invalid_scope"),
        _ => None,
    }
}
fn graph_provider_code(body: &[u8]) -> Option<&'static str> {
    let value: Value = serde_json::from_slice(body).ok()?;
    match value.get("error")?.get("code")?.as_str()? {
        "ErrorAccessDenied" => Some("ErrorAccessDenied"),
        "ErrorInvalidUser" => Some("ErrorInvalidUser"),
        "ErrorMailboxNotEnabledForRESTAPI" => Some("ErrorMailboxNotEnabledForRESTAPI"),
        "ErrorRecipientNotFound" => Some("ErrorRecipientNotFound"),
        _ => None,
    }
}
fn read_secret(path: &str) -> Result<Zeroizing<Vec<u8>>, TransportError> {
    use std::io::Read;
    let path = std::path::Path::new(path);
    if !path.is_absolute() {
        return Err(TransportError::Permanent);
    }
    let link = std::fs::symlink_metadata(path).map_err(|_| TransportError::Permanent)?;
    if !link.file_type().is_file() || link.len() > MAX_SECRET_BYTES {
        return Err(TransportError::Permanent);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path).map_err(|_| TransportError::Permanent)?;
    let meta = file.metadata().map_err(|_| TransportError::Permanent)?;
    if !meta.is_file() || meta.len() > MAX_SECRET_BYTES {
        return Err(TransportError::Permanent);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(TransportError::Permanent);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(TransportError::Permanent);
        }
    }
    let mut data = Zeroizing::new(Vec::new());
    file.take(MAX_SECRET_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|_| TransportError::Permanent)?;
    if data.len() as u64 > MAX_SECRET_BYTES {
        return Err(TransportError::Permanent);
    }
    Ok(data)
}
fn microsoft_form(
    c: &OAuthConfig,
    scope: &str,
) -> Result<(String, Vec<(String, String)>), TransportError> {
    let endpoint = format!("{GRAPH_TOKEN}/{}/oauth2/v2.0/token", c.tenant_id);
    if uuid::Uuid::parse_str(&c.tenant_id).is_err() {
        return Err(TransportError::Permanent);
    }
    let mut form = vec![
        ("client_id".into(), c.client_id.clone()),
        ("grant_type".into(), "client_credentials".into()),
    ];
    match &c.credential {
        OAuthCredential::ClientSecretFile(path) => {
            let secret = read_secret(path)?;
            let secret = std::str::from_utf8(&secret)
                .map_err(|_| TransportError::Permanent)?
                .trim();
            if secret.is_empty() {
                return Err(TransportError::Permanent);
            }
            form.push(("client_secret".into(), secret.to_owned()));
            form.push(("scope".into(), scope.into()));
        }
        OAuthCredential::CertificateAndKeyFiles {
            certificate_file,
            key_file,
        } => {
            let assertion =
                microsoft_certificate_assertion(c, certificate_file, key_file, &endpoint)?;
            form.push(("scope".into(), scope.into()));
            form.push((
                "client_assertion_type".into(),
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
            ));
            form.push(("client_assertion".into(), assertion));
        }
    }
    Ok((endpoint, form))
}

#[derive(Serialize, Deserialize)]
struct MicrosoftClaims<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    jti: String,
    iat: i64,
    nbf: i64,
    exp: i64,
}

fn microsoft_certificate_assertion(
    c: &OAuthConfig,
    certificate_file: &str,
    key_file: &str,
    endpoint: &str,
) -> Result<String, TransportError> {
    use x509_parser::{pem::parse_x509_pem, prelude::FromDer};
    let cert_pem = read_secret(certificate_file)?;
    let key_pem = read_secret(key_file)?;
    let (_, pem) = parse_x509_pem(&cert_pem).map_err(|_| TransportError::Permanent)?;
    let (remaining, cert) = x509_parser::certificate::X509Certificate::from_der(&pem.contents)
        .map_err(|_| TransportError::Permanent)?;
    if !remaining.is_empty() {
        return Err(TransportError::Permanent);
    }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let not_before = cert.validity().not_before.timestamp();
    let not_after = cert.validity().not_after.timestamp();
    if now < not_before || now >= not_after {
        return Err(TransportError::Permanent);
    }
    let public_key = cert.public_key().subject_public_key.data.as_ref();
    let key = EncodingKey::from_rsa_pem(&key_pem).map_err(|_| TransportError::Permanent)?;
    let mut header = Header::new(Algorithm::PS256);
    header.typ = Some("JWT".into());
    header.x5t_s256 = Some(URL_SAFE_NO_PAD.encode(Sha256::digest(&pem.contents)));
    let claims = MicrosoftClaims {
        iss: &c.client_id,
        sub: &c.client_id,
        aud: endpoint,
        jti: Uuid::new_v4().to_string(),
        iat: now,
        nbf: now,
        exp: now.saturating_add(300),
    };
    let assertion =
        jsonwebtoken::encode(&header, &claims, &key).map_err(|_| TransportError::Permanent)?;
    if cert.public_key().algorithm.algorithm.to_id_string() != "1.2.840.113549.1.1.1" {
        return Err(TransportError::Permanent);
    }
    let decoding_key = DecodingKey::from_rsa_der(public_key);
    let mut validation = Validation::new(Algorithm::PS256);
    validation.set_issuer(&[c.client_id.as_str()]);
    validation.set_audience(&[endpoint]);
    validation.validate_exp = true;
    jsonwebtoken::decode::<Value>(&assertion, &decoding_key, &validation)
        .map_err(|_| TransportError::Permanent)?;
    Ok(assertion)
}

#[derive(Serialize, Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
    token_uri: Option<String>,
}
impl Drop for ServiceAccount {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.private_key);
    }
}
#[derive(Serialize)]
struct GoogleClaims<'a> {
    iss: &'a str,
    sub: &'a str,
    scope: &'static str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}
async fn google_form(c: &GoogleConfig) -> Result<(String, Vec<(String, String)>), TransportError> {
    let data = read_secret(&c.service_account_file)?;
    let sa: ServiceAccount =
        serde_json::from_slice(&data).map_err(|_| TransportError::Permanent)?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let claims = GoogleClaims {
        iss: &sa.client_email,
        sub: &c.sender,
        scope: "https://mail.google.com/",
        aud: GOOGLE_TOKEN,
        iat: now,
        exp: now.saturating_add(3600),
    };
    let private_key = Zeroizing::new(sa.private_key.clone());
    let key =
        EncodingKey::from_rsa_pem(private_key.as_bytes()).map_err(|_| TransportError::Permanent)?;
    let mut header = Header::new(Algorithm::RS256);
    header.typ = Some("JWT".into());
    let assertion =
        jsonwebtoken::encode(&header, &claims, &key).map_err(|_| TransportError::Permanent)?;
    let form = vec![
        (
            "grant_type".into(),
            "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
        ),
        ("assertion".into(), assertion),
    ];
    let token_uri = sa
        .token_uri
        .as_ref()
        .filter(|s| s.as_str() == GOOGLE_TOKEN)
        .map(String::as_str)
        .unwrap_or(GOOGLE_TOKEN)
        .to_owned();
    Ok((token_uri, form))
}

pub struct GraphTransport {
    config: GraphConfig,
    provider: TokenProvider,
    status: TransportStatus,
    graph_base_url: Option<String>,
}
impl GraphTransport {
    pub fn new(config: GraphConfig, status: TransportStatus) -> Result<Self, TransportError> {
        let mailbox = config
            .sender
            .parse::<lettre::message::Mailbox>()
            .map_err(|_| TransportError::Permanent)?;
        if mailbox.name.is_some()
            || mailbox.email.to_string() != config.sender
            || !valid_bare_mailbox(&config.sender)
        {
            return Err(TransportError::Permanent);
        }
        Ok(Self {
            provider: TokenProvider::microsoft(
                config.oauth.clone(),
                "https://graph.microsoft.com/.default",
                status.clone(),
            )?,
            config,
            status,
            graph_base_url: None,
        })
    }

    #[cfg(feature = "test-util")]
    pub fn with_test_endpoints(
        config: GraphConfig,
        status: TransportStatus,
        token_url: &str,
        graph_base: &str,
        root_ca: &[u8],
    ) -> Result<Self, TransportError> {
        validate_test_endpoint(token_url)?;
        validate_test_endpoint(graph_base)?;
        preflight_oauth(&config.oauth, &status)?;
        status.configure("graph", &config.sender);
        let mut transport = Self::new(config, status)?;
        transport.provider.client = http_client(Some(root_ca))?;
        transport.provider.token_endpoint = Some(token_url.to_owned());
        transport.graph_base_url = Some(graph_base.to_owned());
        Ok(transport)
    }
}
impl MailTransport for GraphTransport {
    async fn send(&self, m: MailMessage) -> Result<Delivery, TransportError> {
        if !valid_bare_mailbox(&m.recipient) || !valid_header_value(&m.message_id) {
            return Err(TransportError::Permanent);
        }
        let graph_url = self.graph_base_url.as_deref().unwrap_or(GRAPH_SEND);
        let mut endpoint = url::Url::parse(graph_url).map_err(|_| TransportError::Permanent)?;
        endpoint
            .path_segments_mut()
            .map_err(|_| TransportError::Permanent)?
            .push(&self.config.sender)
            .push("sendMail");
        let payload = json!({"message":{"subject":m.subject,"body":{"contentType":"HTML","content":m.html},"toRecipients":[{"emailAddress":{"address":m.recipient}}],"internetMessageHeaders":[{"name":"x-craftcloud-message-id","value":m.message_id}]},"saveToSentItems":self.config.save_to_sent});
        for attempt in 0..2 {
            let token = self.provider.token(attempt == 1).await?;
            let result = self
                .provider
                .client
                .post(endpoint.as_str())
                .bearer_auth(token.as_str())
                .json(&payload)
                .send()
                .await;
            match result {
                Ok(r) if r.status().as_u16() == 401 && attempt == 0 => {
                    self.provider.invalidate().await;
                    continue;
                }
                Ok(r) if r.status().as_u16() == 202 => {
                    self.status.clear_error();
                    return Ok(Delivery);
                }
                Ok(r) => {
                    let status = r.status().as_u16();
                    let retry = parse_retry_after(
                        r.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok()),
                        OffsetDateTime::now_utc().unix_timestamp(),
                    );
                    let body = read_bounded(r, MAX_TOKEN_RESPONSE_BYTES)
                        .await
                        .unwrap_or_else(|_| Zeroizing::new(Vec::new()));
                    let error = provider_error(status, graph_provider_code(&body), retry);
                    self.status.set_error(error.code());
                    return Err(error);
                }
                Err(_) => return Err(TransportError::Transient),
            }
        }
        Err(TransportError::Transient)
    }
}

pub struct SmtpOAuthTransport {
    config: SmtpOAuthConfig,
    provider: TokenProvider,
    status: TransportStatus,
}
impl SmtpOAuthTransport {
    /// Uses a certificate-verified loopback token server in integration builds.
    #[cfg(feature = "test-util")]
    pub fn with_test_endpoints(
        config: SmtpOAuthConfig,
        status: TransportStatus,
        token_url: &str,
        root_ca: &[u8],
    ) -> Result<Self, TransportError> {
        validate_test_endpoint(token_url)?;
        preflight_smtp_oauth(&config, &status)?;
        let mut transport = Self::new(config, status)?;
        transport.provider.client = http_client(Some(root_ca))?;
        transport.provider.token_endpoint = Some(token_url.to_owned());
        Ok(transport)
    }
    fn new(config: SmtpOAuthConfig, status: TransportStatus) -> Result<Self, TransportError> {
        let username = config
            .smtp
            .username
            .as_deref()
            .ok_or(TransportError::Permanent)?;
        let from = config
            .smtp
            .from
            .parse::<lettre::message::Mailbox>()
            .map_err(|_| TransportError::Permanent)?;
        let from_addr = from.email.to_string();
        if !username.eq_ignore_ascii_case(&from_addr) {
            return Err(TransportError::Permanent);
        }
        let provider = match &config.provider {
            OAuthProvider::Microsoft(oauth) => TokenProvider::microsoft(
                oauth.clone(),
                "https://outlook.office365.com/.default",
                status.clone(),
            )?,
            OAuthProvider::Google {
                service_account_file,
                sender,
            } => {
                if !sender.eq_ignore_ascii_case(&from_addr) {
                    return Err(TransportError::Permanent);
                }
                TokenProvider::google(
                    GoogleConfig {
                        service_account_file: service_account_file.clone(),
                        sender: sender.clone(),
                    },
                    status.clone(),
                )?
            }
        };
        Ok(Self {
            provider,
            config,
            status,
        })
    }
}
impl MailTransport for SmtpOAuthTransport {
    async fn send(&self, m: MailMessage) -> Result<Delivery, TransportError> {
        let username = self
            .config
            .smtp
            .username
            .as_deref()
            .ok_or(TransportError::Permanent)?;
        for attempt in 0..2 {
            let token = self.provider.token(attempt == 1).await?;
            let client = self
                .config
                .smtp
                .with_xoauth2_token(username, token.as_str())?;
            let from = self
                .config
                .smtp
                .from
                .parse::<lettre::message::Mailbox>()
                .map_err(|_| TransportError::Permanent)?;
            let recipient = m
                .recipient
                .parse::<lettre::message::Mailbox>()
                .map_err(|_| TransportError::Permanent)?;
            let message = lettre::Message::builder()
                .from(from)
                .to(recipient)
                .subject(m.subject.clone())
                .message_id(Some(m.message_id.clone()))
                .multipart(
                    lettre::message::MultiPart::alternative()
                        .singlepart(
                            lettre::message::SinglePart::builder()
                                .header(lettre::message::header::ContentType::TEXT_PLAIN)
                                .body(m.text.clone()),
                        )
                        .singlepart(
                            lettre::message::SinglePart::builder()
                                .header(lettre::message::header::ContentType::TEXT_HTML)
                                .body(m.html.clone()),
                        ),
                )
                .map_err(|_| TransportError::Permanent)?;
            match client.send(message).await {
                Ok(_) => {
                    self.status.clear_error();
                    return Ok(Delivery);
                }
                Err(error)
                    if crate::smtp::smtp_error(&error) == TransportError::Authentication
                        && attempt == 0 =>
                {
                    self.provider.invalidate().await;
                }
                Err(error) => {
                    let error = crate::smtp::smtp_error(&error);
                    if error == TransportError::Authentication {
                        self.status.set_error("smtp_authentication_failed");
                    }
                    return Err(error);
                }
            }
        }
        Err(TransportError::Authentication)
    }
}

fn valid_bare_mailbox(value: &str) -> bool {
    if value.is_empty()
        || value
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return false;
    }
    let mut parts = value.split('@');
    matches!((parts.next(), parts.next(), parts.next()), (Some(local), Some(domain), None) if !local.is_empty() && !domain.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'))
        && value
            .parse::<lettre::message::Mailbox>()
            .is_ok_and(|m| m.name.is_none() && m.email.to_string() == value)
}
fn valid_header_value(value: &str) -> bool {
    !value.is_empty() && !value.bytes().any(|b| b < 0x20 || b == 0x7f)
}
fn status_code(code: u16) -> &'static str {
    match code {
        400 => "provider_bad_request",
        401 => "provider_unauthorized",
        403 => "provider_forbidden",
        429 => "provider_rate_limited",
        500..=599 => "provider_unavailable",
        _ => "provider_failure",
    }
}
fn provider_error(
    status: u16,
    provider_code: Option<&'static str>,
    retry_after: Option<Duration>,
) -> TransportError {
    let permanent = status < 500 && status != 429;
    let code = provider_code.unwrap_or_else(|| status_code(status));
    TransportError::Provider {
        permanent,
        code,
        retry_after: if status == 429 || (500..=599).contains(&status) {
            retry_after
        } else {
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn configured_smtp_reports_sanitized_delivery_errors() {
        let status = TransportStatus::default();
        let transport = MailTransportConfig::Smtp(SmtpConfig::new(
            "localhost".into(),
            587,
            crate::SmtpTls::StartTls,
            None,
            None,
            "CraftCloud <sender@example.invalid>".into(),
        ))
        .build(status.clone())
        .unwrap();
        let result = transport
            .send(MailMessage {
                recipient: "invalid recipient".into(),
                subject: "synthetic".into(),
                text: "synthetic".into(),
                html: "<p>synthetic</p>".into(),
                message_id: "<test@example.invalid>".into(),
            })
            .await;
        assert!(result.is_err());
        let info = status.snapshot();
        assert_eq!(info.active_transport, "smtp");
        assert_eq!(info.sender, "sender@example.invalid");
        assert_eq!(info.last_safe_error.as_deref(), Some("permanent"));
    }
    #[test]
    fn provider_error_classes_and_retry_after_are_stable() {
        assert!(provider_error(401, Some("invalid_client"), None).is_permanent());
        assert!(!provider_error(429, None, Some(Duration::from_secs(30))).is_permanent());
        assert_eq!(
            provider_error(429, None, Some(Duration::from_secs(30))).retry_after(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(provider_error(403, None, None).code(), "provider_forbidden");
    }
    #[test]
    fn retry_after_accepts_seconds_and_caps_at_one_day() {
        assert_eq!(
            parse_retry_after(Some("12"), 0),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after(Some("999999"), 0),
            Some(Duration::from_secs(86_400))
        );
        assert_eq!(parse_retry_after(Some("bad"), 0), None);
    }
    #[test]
    fn sender_and_recipient_require_bare_mailboxes() {
        assert!(valid_bare_mailbox("user@example.invalid"));
        assert!(!valid_bare_mailbox("Name <user@example.invalid>"));
        assert!(!valid_bare_mailbox(
            "user@example.invalid\r\nBcc:x@y.invalid"
        ));
        assert!(!valid_bare_mailbox("user@localhost"));
    }
    #[test]
    fn provider_error_codes_never_use_unknown_provider_text() {
        assert_eq!(
            token_provider_code(&json!({"error":"invalid_client"})),
            Some("invalid_client")
        );
        assert_eq!(
            token_provider_code(&json!({"error":"secret details"})),
            None
        );
        assert_eq!(
            graph_provider_code(br#"{"error":{"code":"ErrorAccessDenied","message":"secret"}}"#),
            Some("ErrorAccessDenied")
        );
        assert_eq!(
            graph_provider_code(br#"{"error":{"code":"private provider detail"}}"#),
            None
        );
    }
    #[test]
    fn status_contains_no_secret() {
        let s = TransportStatus::default();
        s.configure("graph", "user@example.invalid");
        assert_eq!(s.snapshot().sender, "user@example.invalid");
    }
    #[test]
    #[allow(clippy::unwrap_used)]
    fn credential_loader_rejects_nonregular_paths_and_oversized_files() {
        let directory =
            std::env::temp_dir().join(format!("craftcloud-mail-secret-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("secret");
        std::fs::write(&file, b"synthetic-value").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(read_secret(file.to_str().unwrap()).is_ok());
        assert!(read_secret(directory.to_str().unwrap()).is_err());
        assert!(read_secret("relative-secret-path").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let link = directory.join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(read_secret(link.to_str().unwrap()).is_err());
            std::fs::remove_file(link).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_secret(file.to_str().unwrap()).is_err());
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(MAX_SECRET_BYTES + 1)
            .unwrap();
        assert!(read_secret(file.to_str().unwrap()).is_err());
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn certificate_assertion_rejects_invalid_certificate() {
        let base = std::env::temp_dir().join(format!("craftcloud-cert-{}", Uuid::new_v4()));
        let cert_path = base.with_extension("pem");
        let key_path = base.with_extension("key");
        let result = (|| {
            std::fs::write(&cert_path, b"not a certificate")
                .map_err(|_| TransportError::Permanent)?;
            std::fs::write(&key_path, b"not a key").map_err(|_| TransportError::Permanent)?;
            microsoft_certificate_assertion(
                &OAuthConfig {
                    tenant_id: Uuid::new_v4().to_string(),
                    client_id: "client".into(),
                    credential: OAuthCredential::ClientSecretFile("unused".into()),
                },
                cert_path.to_str().ok_or(TransportError::Permanent)?,
                key_path.to_str().ok_or(TransportError::Permanent)?,
                "https://example.invalid/token",
            )
        })();
        let _ = std::fs::remove_file(cert_path);
        let _ = std::fs::remove_file(key_path);
        assert!(matches!(result, Err(TransportError::Permanent)));
    }
}
