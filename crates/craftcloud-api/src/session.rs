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
use craftcloud_auth::OpaqueToken;
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
    /// The authenticated account.
    pub user_id: Uuid,
    /// Session creation time.
    pub created_at: OffsetDateTime,
    /// Time at which the session expires if it stays idle from now on
    /// (limited by the absolute expiry).
    pub expires_at: OffsetDateTime,
    csrf_token: String,
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
        let Some(token) = session_token(&parts.headers, &services.settings) else {
            return Err(AuthRejection::new(Kind::Unauthenticated, parts));
        };
        let now = services.clock.now();
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

        crate::observe::note_actor(record.user_id);
        if !is_safe_method(&parts.method) && !csrf_ok(&parts.headers, &services.settings, &token) {
            return Err(AuthRejection::new(Kind::CsrfFailed, parts));
        }

        // Throttled activity update; failure must not fail the request.
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
        // Expiry from the activity time that is actually recorded.
        let last_seen = if touched { now } else { record.last_seen_at };
        let idle_expiry = last_seen + services.settings.idle_timeout;
        Ok(Self {
            session_id: record.id,
            user_id: record.user_id,
            created_at: record.created_at,
            expires_at: idle_expiry.min(record.absolute_expires_at),
            csrf_token: token.csrf_token(),
        })
    }
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
