//! Cookie sessions: the reusable [`AuthenticatedSession`] extractor, cookie
//! helpers and the CSRF / Origin checks.
//!
//! Authentication is a random 32-byte token in an `HttpOnly`, `SameSite=Lax`
//! cookie (`__Host-craftcloud_session` with `Secure`; the plain name without
//! `Secure` only for a loopback http public URL during local development).
//! The database stores only the SHA-256 hash of the token.
//!
//! For unsafe methods (anything but GET, HEAD, OPTIONS) the extractor
//! enforces **both**:
//!
//! 1. the `X-CSRF-Token` header must equal (constant time) the CSRF token
//!    derived from the session token, and
//! 2. the `Origin` header, if present, must equal the origin of the
//!    configured public URL; if it is absent the request is rejected when
//!    `Sec-Fetch-Site` is `cross-site`.
//!
//! Failures answer 403 `CSRF_FAILED`. Unauthenticated requests always answer
//! 401 first, so the CSRF check never reveals anything to anonymous callers.
//! The origin is never taken from `Host` or `X-Forwarded-*`.

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{DeviceToken, DeviceTokenKind, OpaqueToken};
use craftcloud_core::policy::OrgRole;
use craftcloud_db::sessions;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::services::AuthSettings;
use crate::{ApiState, REQUEST_ID_HEADER, error_response};

/// Name of the CSRF header.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// A request authenticated by a valid session cookie. Use it as a handler
/// argument; unsafe methods are CSRF-protected automatically.
#[derive(Debug, Clone)]
pub struct AuthenticatedSession {
    /// Session identifier.
    pub session_id: Uuid,
    /// Whether the credential is a device access token.
    pub credential_kind: CredentialKind,
    /// The authenticated account.
    pub user_id: Uuid,
    /// Application ID for an `app` device session; absent for browser and launcher sessions.
    pub app_id: Option<String>,
    /// Origin kind for capability provenance (`app`, `launcher`, `cli`, `browser`).
    pub client_kind: String,
    /// Session creation time.
    pub created_at: OffsetDateTime,
    /// Time at which the session expires if it stays idle from now on
    /// (limited by the absolute expiry).
    pub expires_at: OffsetDateTime,
    csrf_token: String,
}

/// Authentication source for an actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Cookie,
    Device,
}

impl AuthenticatedSession {
    /// The CSRF token clients must send as `X-CSRF-Token` on unsafe requests.
    #[must_use]
    pub fn csrf_token(&self) -> &str {
        &self.csrf_token
    }
}

/// Why authentication or the CSRF check failed.
#[derive(Debug)]
pub struct AuthRejection {
    kind: Kind,
    request_headers: HeaderMap,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Unauthenticated,
    CsrfFailed,
    Unavailable,
    Internal,
    Ambiguous,
    TwoFactorSetupRequired,
}

impl AuthRejection {
    /// Whether the request had no valid session (401).
    #[must_use]
    pub fn is_unauthenticated(&self) -> bool {
        matches!(self.kind, Kind::Unauthenticated)
    }

    fn new(kind: Kind, parts: &Parts) -> Self {
        // Keep only the request id; never retain cookies or other headers.
        let mut request_headers = HeaderMap::new();
        if let Some(id) = parts.headers.get(REQUEST_ID_HEADER) {
            request_headers.insert(REQUEST_ID_HEADER, id.clone());
        }
        Self {
            kind,
            request_headers,
        }
    }
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        let (status, code, message) = match self.kind {
            Kind::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
                "Authentication is required.",
            ),
            Kind::CsrfFailed => (
                StatusCode::FORBIDDEN,
                "CSRF_FAILED",
                "The request failed the cross-site request forgery check.",
            ),
            Kind::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "AUTH_UNAVAILABLE",
                "Authentication is not available because no database is configured or reachable.",
            ),
            Kind::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                "An internal error occurred.",
            ),
            Kind::Ambiguous => (
                StatusCode::BAD_REQUEST,
                "AMBIGUOUS_CREDENTIALS",
                "Send either a session cookie or a bearer token, not both.",
            ),
            Kind::TwoFactorSetupRequired => (
                StatusCode::FORBIDDEN,
                "TWO_FACTOR_SETUP_REQUIRED",
                "Set up two-factor authentication to access this organization.",
            ),
        };
        error_response(status, code, message, &self.request_headers)
    }
}

impl FromRequestParts<ApiState> for AuthenticatedSession {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let Some(services) = state.services.clone() else {
            return Err(AuthRejection::new(Kind::Unavailable, parts));
        };
        let cookie_present = session_cookie_presented(&parts.headers, &services.settings);
        let (bearer, bearer_presented) = device_access_token(&parts.headers);
        if cookie_present && bearer_presented {
            return Err(AuthRejection::new(Kind::Ambiguous, parts));
        }
        let now = services.clock.now();
        let (
            session_id,
            user_id,
            created_at,
            expires_at,
            csrf_token,
            credential_kind,
            app_id,
            client_kind,
        ) = if let Some(token) = bearer {
            let found = craftcloud_db::device_sessions::find_active_access(
                &services.pool,
                &token.hash(),
                now,
                services.settings.touch_interval,
                services.settings.device_idle_timeout,
            )
            .await;
            let record = match found {
                Ok(Some(record)) => record,
                Ok(None) => return Err(AuthRejection::new(Kind::Unauthenticated, parts)),
                Err(error) => {
                    tracing::error!(%error, "device session lookup failed");
                    return Err(AuthRejection::new(Kind::Internal, parts));
                }
            };
            (
                record.id,
                record.user_id,
                record.created_at,
                record
                    .access_expires_at
                    .min(record.idle_expires_at)
                    .min(record.absolute_expires_at),
                String::new(),
                CredentialKind::Device,
                (record.client_kind == "app").then_some(record.client_id),
                record.client_kind,
            )
        } else {
            let Some(token) = session_token(&parts.headers, &services.settings) else {
                return Err(AuthRejection::new(Kind::Unauthenticated, parts));
            };
            let found = sessions::find_active(
                &services.pool,
                &token.hash(),
                now,
                services.settings.idle_timeout,
            )
            .await;
            let record = match found {
                Ok(Some(record)) => record,
                Ok(None) => return Err(AuthRejection::new(Kind::Unauthenticated, parts)),
                Err(error) => {
                    tracing::error!(%error, "session lookup failed");
                    return Err(AuthRejection::new(Kind::Internal, parts));
                }
            };
            if !is_safe_method(&parts.method)
                && !csrf_ok(&parts.headers, &services.settings, &token)
            {
                return Err(AuthRejection::new(Kind::CsrfFailed, parts));
            }
            let touched = match sessions::touch(
                &services.pool,
                record.id,
                now,
                services.settings.touch_interval,
            )
            .await
            {
                Ok(wrote) => wrote,
                Err(error) => {
                    tracing::warn!(%error, "session activity update failed");
                    false
                }
            };
            let last_seen = if touched { now } else { record.last_seen_at };
            (
                record.id,
                record.user_id,
                record.created_at,
                (last_seen + services.settings.idle_timeout).min(record.absolute_expires_at),
                token.csrf_token(),
                CredentialKind::Cookie,
                None,
                "browser".to_owned(),
            )
        };

        match organization_requires_two_factor(&services, user_id, parts.uri.path()).await {
            Ok(true) => return Err(AuthRejection::new(Kind::TwoFactorSetupRequired, parts)),
            Ok(false) => {}
            Err(AuthRejectionKind::Internal) => {
                return Err(AuthRejection::new(Kind::Internal, parts));
            }
        }

        crate::observe::note_actor(user_id);
        Ok(Self {
            session_id,
            credential_kind,
            user_id,
            app_id,
            client_kind,
            created_at,
            expires_at,
            csrf_token,
        })
    }
}

async fn organization_requires_two_factor(
    services: &crate::services::Services,
    user_id: Uuid,
    path: &str,
) -> Result<bool, AuthRejectionKind> {
    // Axum route captures are percent-decoded. Decode each raw URI segment
    // separately so encoded slashes cannot change the route structure.
    // An undecodable segment cannot name an organization; the router or
    // extractor rejects such a path normally.
    let Some(decoded) = path
        .split('/')
        .filter(|value| !value.is_empty())
        .map(decode_path_segment)
        .collect::<Option<Vec<String>>>()
    else {
        return Ok(false);
    };
    let segments: Vec<&str> = decoded.iter().map(String::as_str).collect();
    // Instance-admin routes carry organization ids but are not organization
    // scoped; the organization policy must not apply to them.
    if matches!(segments.as_slice(), ["api", "v1", "admin", ..]) {
        return Ok(false);
    }
    let id_after = |name: &str| -> Option<Uuid> {
        let index = segments.iter().position(|segment| *segment == name)?;
        Uuid::parse_str(segments.get(index + 1)?).ok()
    };
    let organization_id = if let Some(id) = id_after("organizations") {
        Some(id)
    } else if let Some(id) = id_after("workspaces") {
        sqlx_scalar_org(services, "SELECT org_id FROM workspaces WHERE id=$1", id).await?
    } else if let Some(id) = id_after("projects") {
        sqlx_scalar_org(services, "SELECT org_id FROM projects WHERE id=$1", id).await?
    } else if let Some(id) = id_after("files") {
        sqlx_scalar_org(services, "SELECT org_id FROM files WHERE id=$1", id).await?
    } else if let Some(id) = id_after("uploads") {
        sqlx_scalar_org(
            services,
            "SELECT org_id FROM upload_sessions WHERE id=$1",
            id,
        )
        .await?
    } else if let Some(id) = id_after("grants") {
        sqlx_scalar_org(
            services,
            "SELECT org_id FROM grants WHERE id=$1 AND revoked_at IS NULL",
            id,
        )
        .await?
    } else {
        None
    };
    let Some(org_id) = organization_id else {
        return Ok(false);
    };
    let member = craftcloud_db::organizations::active_role(&services.pool, org_id, user_id)
        .await
        .map_err(|error| {
            tracing::error!(%error,"2FA organization membership lookup failed");
            AuthRejectionKind::Internal
        })?;
    if member.is_none() {
        return Ok(false);
    }
    if !route_resource_visible(services, user_id, org_id, member, &segments, path).await? {
        return Ok(false);
    }
    let required: Option<bool> =
        sqlx::query_scalar("SELECT require_two_factor FROM organizations WHERE id=$1")
            .bind(org_id)
            .fetch_optional(&services.pool)
            .await
            .map_err(|error| {
                tracing::error!(%error,"2FA organization policy lookup failed");
                AuthRejectionKind::Internal
            })?;
    if required != Some(true) {
        return Ok(false);
    }
    let (enabled, _) = craftcloud_db::two_factor::status(&services.pool, user_id)
        .await
        .map_err(|error| {
            tracing::error!(%error,"2FA status lookup failed");
            AuthRejectionKind::Internal
        })?;
    Ok(!enabled)
}

fn decode_path_segment(segment: &str) -> Option<String> {
    let input = segment.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while let Some(&byte) = input.get(index) {
        if byte == b'%' {
            let high = *input.get(index + 1)?;
            let low = *input.get(index + 2)?;
            decoded.push((hex_nibble(high)? << 4) | hex_nibble(low)?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

async fn route_resource_visible(
    services: &crate::services::Services,
    user_id: Uuid,
    org_id: Uuid,
    role: Option<OrgRole>,
    segments: &[&str],
    path: &str,
) -> Result<bool, AuthRejectionKind> {
    let role = role.ok_or(AuthRejectionKind::Internal)?;
    let privileged = matches!(role, OrgRole::Owner | OrgRole::Admin);
    let active = |query: &'static str, id: Uuid| async move {
        sqlx::query_scalar::<_, bool>(query)
            .bind(id)
            .bind(user_id)
            .fetch_one(&services.pool)
            .await
    };
    if let Some(index) = segments.iter().position(|value| *value == "organizations") {
        let tail = segments.get(index + 2).copied();
        return Ok(match tail {
            None => !matches!(role, OrgRole::Guest),
            Some("workspaces" | "shared-with-me") => true,
            Some("members") => !matches!(role, OrgRole::Guest),
            Some("audit" | "usage" | "invitations" | "overview") => privileged,
            _ => !matches!(role, OrgRole::Guest),
        });
    }
    if let Some(workspace_id) = segments
        .iter()
        .position(|value| *value == "workspaces")
        .and_then(|i| segments.get(i + 1))
        .and_then(|id| Uuid::parse_str(id).ok())
    {
        let exists = active(
            "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=$1 AND user_id=$2)",
            workspace_id,
        )
        .await
        .map_err(|error| {
            tracing::error!(%error,"workspace visibility lookup failed");
            AuthRejectionKind::Internal
        })?;
        return Ok(privileged || exists);
    }
    if let Some(project_id) = segments
        .iter()
        .position(|value| *value == "projects")
        .and_then(|i| segments.get(i + 1))
        .and_then(|id| Uuid::parse_str(id).ok())
    {
        let row: Option<(Uuid,)> =
            sqlx::query_as("SELECT workspace_id FROM projects WHERE id=$1 AND org_id=$2")
                .bind(project_id)
                .bind(org_id)
                .fetch_optional(&services.pool)
                .await
                .map_err(|error| {
                    tracing::error!(%error,"project visibility lookup failed");
                    AuthRejectionKind::Internal
                })?;
        let Some((workspace_id,)) = row else {
            return Ok(false);
        };
        let in_workspace = active(
            "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=$1 AND user_id=$2)",
            workspace_id,
        )
        .await
        .map_err(|error| {
            tracing::error!(%error,"project workspace visibility lookup failed");
            AuthRejectionKind::Internal
        })?;
        let granted = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM grants WHERE org_id=$1 AND target_kind='project' AND target_id=$2 AND grantee_user_id=$3 AND revoked_at IS NULL)")
            .bind(org_id).bind(project_id).bind(user_id).fetch_one(&services.pool).await
            .map_err(|error| { tracing::error!(%error,"project grant visibility lookup failed"); AuthRejectionKind::Internal })?;
        return Ok(privileged || in_workspace || granted);
    }
    if let Some(file_id) = segments
        .iter()
        .position(|value| *value == "files")
        .and_then(|i| segments.get(i + 1))
        .and_then(|id| Uuid::parse_str(id).ok())
    {
        let row: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT workspace_id,project_id FROM files WHERE id=$1 AND org_id=$2")
                .bind(file_id)
                .bind(org_id)
                .fetch_optional(&services.pool)
                .await
                .map_err(|error| {
                    tracing::error!(%error,"file visibility lookup failed");
                    AuthRejectionKind::Internal
                })?;
        let Some((workspace_id, project_id)) = row else {
            return Ok(false);
        };
        let in_workspace = active(
            "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=$1 AND user_id=$2)",
            workspace_id,
        )
        .await
        .map_err(|error| {
            tracing::error!(%error,"file workspace visibility lookup failed");
            AuthRejectionKind::Internal
        })?;
        let granted = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM grants WHERE org_id=$1 AND grantee_user_id=$2 AND revoked_at IS NULL AND ((target_kind='file' AND target_id=$3) OR (target_kind='project' AND target_id=$4)) )")
            .bind(org_id).bind(user_id).bind(file_id).bind(project_id).fetch_one(&services.pool).await
            .map_err(|error| { tracing::error!(%error,"file grant visibility lookup failed"); AuthRejectionKind::Internal })?;
        return Ok(privileged || in_workspace || granted);
    }
    if let Some(upload_id) = segments
        .iter()
        .position(|value| *value == "uploads")
        .and_then(|i| segments.get(i + 1))
        .and_then(|id| Uuid::parse_str(id).ok())
    {
        let visible = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM upload_sessions WHERE id=$1 AND org_id=$2 AND (created_by_user_id=$3 OR EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=upload_sessions.workspace_id AND user_id=$3)))")
            .bind(upload_id).bind(org_id).bind(user_id).fetch_one(&services.pool).await
            .map_err(|error| { tracing::error!(%error,"upload visibility lookup failed"); AuthRejectionKind::Internal })?;
        return Ok(privileged || visible);
    }
    if let Some(grant_id) = segments
        .iter()
        .position(|value| *value == "grants")
        .and_then(|i| segments.get(i + 1))
        .and_then(|id| Uuid::parse_str(id).ok())
    {
        let visible = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM grants g WHERE g.id=$1 AND g.org_id=$2 AND g.revoked_at IS NULL AND (g.grantee_user_id=$3 OR EXISTS(SELECT 1 FROM workspace_members wm WHERE wm.workspace_id=g.workspace_id AND wm.user_id=$3)))")
            .bind(grant_id).bind(org_id).bind(user_id).fetch_one(&services.pool).await
            .map_err(|error| { tracing::error!(%error,"grant visibility lookup failed"); AuthRejectionKind::Internal })?;
        return Ok(privileged || visible);
    }
    let _ = path;
    Ok(true)
}

#[derive(Debug)]
enum AuthRejectionKind {
    Internal,
}

async fn sqlx_scalar_org(
    services: &crate::services::Services,
    query: &str,
    id: Uuid,
) -> Result<Option<Uuid>, AuthRejectionKind> {
    sqlx::query_scalar::<_, Uuid>(query)
        .bind(id)
        .fetch_optional(&services.pool)
        .await
        .map_err(|error| {
            tracing::error!(%error,"2FA resource scope lookup failed");
            AuthRejectionKind::Internal
        })
}

fn device_access_token(headers: &HeaderMap) -> (Option<DeviceToken>, bool) {
    let values: Vec<_> = headers.get_all(header::AUTHORIZATION).iter().collect();
    let bearer_presented = values.iter().any(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| value.split_once(' '))
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
    });
    if values.len() != 1 {
        return (None, bearer_presented);
    }
    let Some(value) = values.first().and_then(|v| v.to_str().ok()) else {
        return (None, false);
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return (None, false);
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return (None, false);
    }
    if token.is_empty() || token.bytes().any(|b| b.is_ascii_whitespace()) {
        return (None, true);
    }
    (DeviceToken::parse(token, DeviceTokenKind::Access), true)
}

pub(crate) fn ambiguous_credentials(headers: &HeaderMap, settings: &AuthSettings) -> bool {
    session_cookie_presented(headers, settings) && device_access_token(headers).1
}

fn is_safe_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Finds the session cookie among all `Cookie` headers. Headers are scanned
/// as bytes and every `name=value` pair is judged on its own, so an unreadable
/// pair (non-UTF8, no `=`) never hides the session pair next to it. Returns how
/// many pairs carry the session cookie name and the value of the last one.
fn scan_session_cookie<'a>(
    headers: &'a HeaderMap,
    settings: &AuthSettings,
) -> (usize, Option<&'a [u8]>) {
    let name = settings.cookie_name().as_bytes();
    let mut count = 0;
    let mut value = None;
    for header_value in headers.get_all(header::COOKIE) {
        for pair in header_value.as_bytes().split(|b| *b == b';') {
            let pair = pair.trim_ascii();
            if let Some(eq) = pair.iter().position(|b| *b == b'=')
                && let (Some(key), Some(val)) = (pair.get(..eq), pair.get(eq + 1..))
                && key == name
            {
                count += 1;
                value = Some(val);
            }
        }
    }
    (count, value)
}

/// The session token from the `Cookie` header. Several cookies with the
/// session name are ambiguous (cookie tossing) and count as no session.
pub(crate) fn session_token(headers: &HeaderMap, settings: &AuthSettings) -> Option<OpaqueToken> {
    match scan_session_cookie(headers, settings) {
        (1, Some(value)) => OpaqueToken::parse(std::str::from_utf8(value).ok()?),
        _ => None,
    }
}

/// Whether the request carried a cookie with the session name at all (even a
/// malformed one). Used to decide whether a clearing `Set-Cookie` makes sense.
pub(crate) fn session_cookie_presented(headers: &HeaderMap, settings: &AuthSettings) -> bool {
    scan_session_cookie(headers, settings).0 > 0
}

/// CSRF header check plus Origin check, both required.
fn csrf_ok(headers: &HeaderMap, settings: &AuthSettings, token: &OpaqueToken) -> bool {
    let header_ok = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|candidate| token.csrf_matches(candidate));
    header_ok && origin_ok(headers, settings)
}

/// Origin check shared with login: a present `Origin` must equal the public
/// origin; an absent one is rejected if the browser says `cross-site`.
pub(crate) fn origin_ok(headers: &HeaderMap, settings: &AuthSettings) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    match (origins.next(), origins.next()) {
        (Some(origin), None) => match (origin.to_str().ok(), settings.public_origin.as_deref()) {
            (Some(given), Some(expected)) => given == expected,
            _ => false,
        },
        (Some(_), Some(_)) => false,
        (None, _) => !headers
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|site| site.eq_ignore_ascii_case("cross-site")),
    }
}

/// `Set-Cookie` value that sets the session cookie.
pub(crate) fn session_cookie(
    settings: &AuthSettings,
    token: &OpaqueToken,
    max_age_secs: i64,
) -> Option<HeaderValue> {
    cookie_value(settings, token.expose(), max_age_secs.max(0))
}

/// `Set-Cookie` value that clears the session cookie.
pub(crate) fn cleared_cookie(settings: &AuthSettings) -> Option<HeaderValue> {
    cookie_value(settings, "", 0)
}

fn cookie_value(settings: &AuthSettings, value: &str, max_age_secs: i64) -> Option<HeaderValue> {
    let mut cookie = format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}",
        settings.cookie_name()
    );
    if settings.cookie_secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    fn settings(origin: Option<&str>) -> AuthSettings {
        AuthSettings {
            public_origin: origin.map(str::to_owned),
            ..AuthSettings::default()
        }
    }

    #[test]
    fn scope_path_decodes_each_segment_without_changing_encoded_slashes() {
        assert_eq!(decode_path_segment("%31%32%33"), Some("123".to_owned()));
        assert_eq!(decode_path_segment("a%2Fb"), Some("a/b".to_owned()));
        assert_eq!(decode_path_segment("%GG"), None);
        let segments: Vec<_> = "/files/a%2Fb"
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        assert_eq!(segments.len(), 2);
        assert_eq!(decode_path_segment(segments[1]), Some("a/b".to_owned()));
    }

    #[test]
    fn origin_rules() {
        let s = settings(Some("https://cloud.example.org"));
        assert!(origin_ok(
            &headers(&[("origin", "https://cloud.example.org")]),
            &s
        ));
        assert!(!origin_ok(
            &headers(&[("origin", "https://evil.example")]),
            &s
        ));
        assert!(!origin_ok(&headers(&[("origin", "null")]), &s));
        assert!(!origin_ok(
            &headers(&[("origin", "https://cloud.example.org:8443")]),
            &s
        ));
        assert!(!origin_ok(
            &headers(&[("origin", "http://cloud.example.org")]),
            &s
        ));
        assert!(!origin_ok(
            &headers(&[
                ("origin", "https://cloud.example.org"),
                ("origin", "https://cloud.example.org")
            ]),
            &s
        ));
        // No Origin: allowed unless the browser says cross-site.
        assert!(origin_ok(&headers(&[]), &s));
        assert!(origin_ok(
            &headers(&[("sec-fetch-site", "same-origin")]),
            &s
        ));
        assert!(origin_ok(&headers(&[("sec-fetch-site", "none")]), &s));
        assert!(!origin_ok(
            &headers(&[("sec-fetch-site", "cross-site")]),
            &s
        ));
        assert!(!origin_ok(
            &headers(&[("sec-fetch-site", "Cross-Site")]),
            &s
        ));
        // Without a configured origin every browser Origin fails closed.
        let none = settings(None);
        assert!(!origin_ok(
            &headers(&[("origin", "https://cloud.example.org")]),
            &none
        ));
        assert!(origin_ok(&headers(&[]), &none));
    }

    #[test]
    fn cookie_attributes() {
        let secure = AuthSettings::default();
        let token = OpaqueToken::parse(&"a".repeat(43)).unwrap();
        let set = session_cookie(&secure, &token, 3600).unwrap();
        let text = set.to_str().unwrap();
        assert!(text.starts_with("__Host-craftcloud_session="));
        for attr in [
            "Path=/",
            "HttpOnly",
            "SameSite=Lax",
            "Max-Age=3600",
            "Secure",
        ] {
            assert!(text.contains(attr), "{attr} missing in {text}");
        }
        assert!(!text.contains("Domain"));

        let local = AuthSettings {
            cookie_secure: false,
            ..AuthSettings::default()
        };
        let text = session_cookie(&local, &token, 10).unwrap();
        let text = text.to_str().unwrap();
        assert!(text.starts_with("craftcloud_session="));
        assert!(!text.contains("Secure"));
        assert!(text.contains("HttpOnly") && text.contains("SameSite=Lax"));

        let cleared = cleared_cookie(&secure).unwrap();
        let cleared = cleared.to_str().unwrap();
        assert!(cleared.starts_with("__Host-craftcloud_session=;"));
        assert!(cleared.contains("Max-Age=0") && cleared.contains("Secure"));
    }

    #[test]
    fn session_token_parsing() {
        let s = AuthSettings::default();
        let good = "a".repeat(43);
        let one = headers(&[(
            "cookie",
            &format!("other=1; __Host-craftcloud_session={good}; x=y"),
        )]);
        assert!(session_token(&one, &s).is_some());
        // Wrong name (the plain name is not accepted when Secure is on).
        let plain = headers(&[("cookie", &format!("craftcloud_session={good}"))]);
        assert!(session_token(&plain, &s).is_none());
        // Duplicate cookies of the session name are ambiguous.
        let dup = headers(&[(
            "cookie",
            &format!("__Host-craftcloud_session={good}; __Host-craftcloud_session={good}"),
        )]);
        assert!(session_token(&dup, &s).is_none());
        // Malformed value / absent cookie.
        let bad = headers(&[("cookie", "__Host-craftcloud_session=short")]);
        assert!(session_token(&bad, &s).is_none());
        // Unreadable bytes in the same header, before the session pair.
        let mut same = HeaderMap::new();
        let mut raw = b"a=".to_vec();
        raw.extend_from_slice(&[0xff, 0xfe, 0x80]);
        raw.extend_from_slice(format!("; __Host-craftcloud_session={good}; z=1").as_bytes());
        same.append(header::COOKIE, HeaderValue::from_bytes(&raw).unwrap());
        assert!(session_token(&same, &s).is_some());
        assert!(session_cookie_presented(&same, &s));
        assert!(!session_cookie_presented(
            &headers(&[("cookie", "other=1")]),
            &s
        ));
        assert!(session_cookie_presented(&bad, &s));
        // Invalid text in another Cookie header does not hide the session.
        let mut tolerant = HeaderMap::new();
        tolerant.append(
            header::COOKIE,
            HeaderValue::from_bytes(&[b'x', b'=', 0xff, 0xfe]).unwrap(),
        );
        tolerant.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("junk; =; {}={good}", "__Host-craftcloud_session"))
                .unwrap(),
        );
        assert!(session_token(&tolerant, &s).is_some());
        assert!(session_token(&headers(&[]), &s).is_none());
    }

    #[test]
    fn safe_methods() {
        assert!(is_safe_method(&Method::GET));
        assert!(is_safe_method(&Method::HEAD));
        assert!(!is_safe_method(&Method::POST));
        assert!(!is_safe_method(&Method::DELETE));
        assert!(!is_safe_method(&Method::PUT));
    }
}
