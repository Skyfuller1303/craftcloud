//! OIDC browser handoff, account proof and native completion capabilities.
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::session::origin_ok;
use crate::{ApiState, ClientIp, Services, error_response};
use axum::{
    Extension, Json,
    extract::{
        Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use craftcloud_auth::{DeviceToken, DeviceTokenKind, OpaqueToken};
use craftcloud_core::NormalizedEmail;
use craftcloud_db::{oidc as db, oidc_state as flows, sso, two_factor};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

#[derive(Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct Desktop {
    client_kind: String,
    client_id: String,
    device_name: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Start {
    organization_id: Uuid,
    desktop: Option<Desktop>,
}
#[derive(Serialize, Deserialize)]
struct Launch {
    authorization_url: String,
    issuer: String,
    nonce: String,
    pkce_verifier: String,
    desktop: Option<Desktop>,
}
#[derive(Serialize, Deserialize)]
struct Claims {
    issuer: String,
    subject: String,
    email: Option<String>,
    email_verified: bool,
    display_name: Option<String>,
    mfa: bool,
}
impl Claims {
    fn identity(&self) -> db::Identity {
        db::Identity {
            issuer: self.issuer.clone(),
            subject: self.subject.clone(),
            email: self.email.clone(),
            email_verified: self.email_verified,
            display_name: self.display_name.clone(),
            mfa: self.mfa,
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Pending {
    claims: Claims,
    desktop: Option<Desktop>,
}
#[derive(Serialize, Deserialize)]
struct DeviceGrant {
    session_id: Uuid,
    user_id: Uuid,
    reply: Value,
}
fn invalid(h: &HeaderMap) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "SSO_INVALID_FLOW",
        "Start company sign-in again.",
        h,
    )
}
fn hash(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}
fn encrypt(s: &Services, org: Uuid, purpose: &[u8], value: &impl Serialize) -> Option<Vec<u8>> {
    let bytes = Zeroizing::new(serde_json::to_vec(value).ok()?);
    craftcloud_auth::encrypt_secret_for(s.two_factor_keys.current.as_ref()?, org, purpose, &bytes)
        .ok()
}
fn decrypt<T: serde::de::DeserializeOwned>(
    s: &Services,
    org: Uuid,
    purpose: &[u8],
    value: &[u8],
) -> Option<T> {
    let bytes =
        s.two_factor_keys
            .current
            .as_ref()
            .and_then(|key| craftcloud_auth::decrypt_secret_for(key, value, org, purpose).ok())
            .or_else(|| {
                s.two_factor_keys.old.as_ref().and_then(|key| {
                    craftcloud_auth::decrypt_secret_for(key, value, org, purpose).ok()
                })
            })?;
    serde_json::from_slice(&Zeroizing::new(bytes)).ok()
}
fn cookie_name(s: &Services) -> &'static str {
    if s.settings.cookie_secure {
        "__Host-craftcloud_oidc"
    } else {
        "craftcloud_oidc"
    }
}
fn binding(h: &HeaderMap, s: &Services) -> Option<[u8; 32]> {
    let mut values = h
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|v| v.trim().split_once('='))
        .filter(|(k, _)| *k == cookie_name(s))
        .map(|(_, v)| v);
    let raw = values.next()?;
    if values.next().is_some() {
        return None;
    }
    Some(OpaqueToken::parse(raw)?.hash())
}
fn set_binding(mut r: Response, s: &Services, value: &str, age: i32) -> Response {
    let secure = if s.settings.cookie_secure {
        "; Secure"
    } else {
        ""
    };
    if let Ok(v) = HeaderValue::from_str(&format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={age}{secure}",
        cookie_name(s)
    )) {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}
fn redirect(target: &str) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(target) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}
fn desktop_valid(d: &Desktop) -> bool {
    let Ok(u) = url::Url::parse(&d.redirect_uri) else {
        return false;
    };
    let loopback = match u.host() {
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        _ => false,
    };
    loopback
        && u.scheme() == "http"
        && u.port().is_some_and(|p| p > 0)
        && u.path() == "/oidc/callback"
        && u.query().is_none()
        && u.fragment().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && matches!(d.client_kind.as_str(), "app" | "launcher" | "cli")
        && !d.client_id.is_empty()
        && d.client_id.len() <= 32
        && d.client_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !d.device_name.trim().is_empty()
        && d.device_name.chars().count() <= 64
        && !d.device_name.chars().any(char::is_control)
        && d.code_challenge.len() == 43
        && URL_SAFE_NO_PAD
            .decode(&d.code_challenge)
            .is_ok_and(|v| v.len() == 32)
        && d.state.len() >= 32
        && d.state.len() <= 128
        && d.state
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Discover {
    email: String,
}
pub(crate) async fn discover(
    State(state): State<ApiState>,
    ip: Option<Extension<ClientIp>>,
    h: HeaderMap,
    b: Result<Json<Discover>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&h, "AUTH_UNAVAILABLE");
    };
    if !origin_ok(&h, &s.settings) {
        return error_response(
            StatusCode::FORBIDDEN,
            "CSRF_FAILED",
            "The request origin is not allowed.",
            &h,
        );
    }
    if let Err(r) = s.rate_limit_ip(crate::services::IpClass::Login, ip.map(|v| v.0.0), &h) {
        return *r;
    }
    let b = match b {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &h),
    };
    let org = match NormalizedEmail::parse(&b.email) {
        Ok(e) => match sso::discover_domain(
            &s.pool,
            e.as_str().rsplit_once('@').map(|(_, d)| d).unwrap_or(""),
        )
        .await
        {
            Ok(v) => v,
            Err(_) => return internal(&h),
        },
        Err(_) => None,
    };
    Json(json!({"organization_id":org})).into_response()
}
pub(crate) async fn start(
    State(state): State<ApiState>,
    ip: Option<Extension<ClientIp>>,
    h: HeaderMap,
    b: Result<Json<Start>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&h, "AUTH_UNAVAILABLE");
    };
    if !origin_ok(&h, &s.settings) {
        return error_response(
            StatusCode::FORBIDDEN,
            "CSRF_FAILED",
            "The request origin is not allowed.",
            &h,
        );
    }
    if let Err(r) = s.rate_limit_ip(crate::services::IpClass::Login, ip.map(|v| v.0.0), &h) {
        return *r;
    }
    let b = match b {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &h),
    };
    if b.desktop.as_ref().is_some_and(|d| !desktop_valid(d)) {
        return validation_failed(&["desktop"], &h);
    }
    if !sso::instance_enabled(&s.pool).await.unwrap_or(false) {
        return unavailable(&h, "SSO_UNAVAILABLE");
    }
    let c = match sso::configuration(&s.pool, b.organization_id).await {
        Ok(Some(c)) if c.enabled => c,
        _ => return unavailable(&h, "SSO_UNAVAILABLE"),
    };
    let settings = match crate::sso_settings::provider_settings(&s, &c) {
        Ok(v) => v,
        Err(_) => return unavailable(&h, "SSO_UNAVAILABLE"),
    };
    let a = match crate::oidc_provider::authorize(&settings).await {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "SSO_PROVIDER_UNAVAILABLE",
                "The identity provider could not be verified.",
                &h,
            );
        }
    };
    let flow = match OpaqueToken::generate() {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    let launch = Launch {
        authorization_url: a.authorization_url,
        issuer: a.issuer,
        nonce: a.nonce,
        pkce_verifier: a.pkce_verifier,
        desktop: b.desktop,
    };
    let Some(cipher) = encrypt(&s, c.org_id, b"oidc-state", &launch) else {
        return unavailable(&h, "SSO_UNAVAILABLE");
    };
    match flows::insert_state(
        &s.pool,
        &hash(&a.state),
        &flow.hash(),
        c.org_id,
        c.version,
        &[0; 32],
        &cipher,
        s.clock.now(),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "Too many company sign-ins are pending.",
                &h,
            );
        }
        Err(_) => return internal(&h),
    }
    let Some(base) = s.settings.public_url.as_deref() else {
        return unavailable(&h, "SSO_UNAVAILABLE");
    };
    Json(json!({"authorization_url":format!("{}/api/v1/auth/oidc/authorize?flow={}",base.trim_end_matches('/'),flow.expose())})).into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizeQuery {
    flow: String,
}
pub(crate) async fn authorize(
    State(state): State<ApiState>,
    h: HeaderMap,
    q: Result<Query<AuthorizeQuery>, QueryRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&h, "AUTH_UNAVAILABLE");
    };
    let q = match q {
        Ok(Query(q)) => q,
        Err(_) => return invalid(&h),
    };
    let Some(flow) = OpaqueToken::parse(&q.flow) else {
        return invalid(&h);
    };
    let cookie = match OpaqueToken::generate() {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    let row = match flows::claim_launch(&s.pool, &flow.hash(), &cookie.hash(), s.clock.now()).await
    {
        Ok(Some(v)) => v,
        _ => return invalid(&h),
    };
    let Some(launch) = decrypt::<Launch>(&s, row.org_id, b"oidc-state", &row.secrets_ciphertext)
    else {
        return invalid(&h);
    };
    set_binding(
        redirect(&launch.authorization_url),
        &s,
        cookie.expose(),
        600,
    )
}
#[derive(Deserialize)]
// OIDC authorization responses may contain additional provider parameters.
pub(crate) struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
    session_state: Option<String>,
    iss: Option<String>,
}
pub(crate) async fn callback(
    State(state): State<ApiState>,
    h: HeaderMap,
    q: Result<Query<Callback>, QueryRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&h, "AUTH_UNAVAILABLE");
    };
    let q = match q {
        Ok(Query(q)) => q,
        Err(_) => return invalid(&h),
    };
    if q.state.len() > 256
        || q.error.as_ref().is_some_and(|v| v.len() > 256)
        || q.session_state.as_ref().is_some_and(|v| v.len() > 1024)
        || q.iss.as_ref().is_some_and(|v| v.len() > 2048)
    {
        return invalid(&h);
    }
    let Some(binding) = binding(&h, &s) else {
        return invalid(&h);
    };
    let row = match flows::consume_state(&s.pool, &hash(&q.state), &binding, s.clock.now()).await {
        Ok(Some(v)) => v,
        _ => return invalid(&h),
    };
    if q.error.is_some() {
        return invalid(&h);
    }
    let Some(code) = q.code.filter(|v| !v.is_empty() && v.len() <= 4096) else {
        return invalid(&h);
    };
    let Some(launch) = decrypt::<Launch>(&s, row.org_id, b"oidc-state", &row.secrets_ciphertext)
    else {
        return invalid(&h);
    };
    if q.iss.as_deref().is_some_and(|iss| iss != launch.issuer) {
        return invalid(&h);
    }
    let c = match sso::configuration(&s.pool, row.org_id).await {
        Ok(Some(c)) if c.enabled && c.version == row.config_version => c,
        _ => return invalid(&h),
    };
    let settings = match crate::sso_settings::provider_settings(&s, &c) {
        Ok(v) => v,
        Err(_) => return unavailable(&h, "SSO_UNAVAILABLE"),
    };
    let identity = match crate::oidc_provider::exchange(
        &settings,
        &launch.issuer,
        &code,
        &launch.nonce,
        &launch.pkce_verifier,
    )
    .await
    {
        Ok(i) => i,
        Err(_) => return invalid(&h),
    };
    let p = Pending {
        claims: Claims {
            issuer: identity.issuer,
            subject: identity.subject,
            email: identity.email,
            email_verified: identity.email_verified,
            display_name: identity.display_name,
            mfa: identity.mfa,
        },
        desktop: launch.desktop,
    };
    let token = match OpaqueToken::generate() {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    let Some(cipher) = encrypt(&s, row.org_id, b"oidc-pending", &p) else {
        return internal(&h);
    };
    let mut tx = match s.pool.begin().await {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    match flows::insert_pending(
        &mut tx,
        &token.hash(),
        row.org_id,
        row.config_version,
        &binding,
        &cipher,
        s.clock.now(),
        600,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "Too many sign-ins are pending.",
                &h,
            );
        }
        Err(_) => return internal(&h),
    }
    if tx.commit().await.is_err() {
        return internal(&h);
    }
    redirect(&format!("/admin/#oidc={}", token.expose()))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Finish {
    pending_id: String,
    password: Option<String>,
    code: Option<String>,
    recovery_code: Option<String>,
}
fn needed(h: &HeaderMap, code: &str) -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        code,
        "Complete the additional account proof to continue.",
        h,
    )
}
async fn factor(
    s: &Services,
    user: Uuid,
    code: Option<&str>,
    recovery: Option<&str>,
) -> Option<(two_factor::VerifiedFactor, Vec<u8>)> {
    let record = two_factor::secret(&s.pool, user).await.ok()??;
    if record.enabled_at.is_none() || record.failed_attempts >= two_factor::MAX_FAILED_ATTEMPTS {
        return None;
    }
    let proof = match (code, recovery) {
        (Some(c), None) => {
            let secret = crate::two_factor::decrypt_for_keys(s, user, &record.secret_ciphertext)?;
            craftcloud_auth::verify_totp(
                &secret,
                c,
                s.clock.now().unix_timestamp(),
                record.last_accepted_step,
            )
            .ok()
            .flatten()
            .map(two_factor::VerifiedFactor::TotpStep)
        }
        (None, Some(c)) => crate::two_factor::verify_recovery_code(s, user, c).await,
        _ => None,
    }?;
    Some((proof, record.secret_ciphertext))
}
pub(crate) async fn finish(
    State(state): State<ApiState>,
    ip: Option<Extension<ClientIp>>,
    h: HeaderMap,
    b: Result<Json<Finish>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&h, "AUTH_UNAVAILABLE");
    };
    if !origin_ok(&h, &s.settings) {
        return error_response(
            StatusCode::FORBIDDEN,
            "CSRF_FAILED",
            "The request origin is not allowed.",
            &h,
        );
    }
    if let Err(r) = s.rate_limit_ip(crate::services::IpClass::Login, ip.map(|v| v.0.0), &h) {
        return *r;
    }
    let mut b = match b {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &h),
    };
    let password = Zeroizing::new(b.password.take().unwrap_or_default());
    if password.len() > 1024
        || b.code.as_ref().is_some_and(|v| v.len() > 16)
        || b.recovery_code.as_ref().is_some_and(|v| v.len() > 128)
        || (b.code.is_some() && b.recovery_code.is_some())
    {
        return validation_failed(&["proof"], &h);
    }
    let Some(token) = OpaqueToken::parse(&b.pending_id) else {
        return invalid(&h);
    };
    let Some(browser_binding) = binding(&h, &s) else {
        return invalid(&h);
    };
    let row = match flows::pending(&s.pool, &token.hash(), s.clock.now()).await {
        Ok(Some(v)) => v,
        _ => return invalid(&h),
    };
    if !bool::from(row.binding_hash.as_slice().ct_eq(&browser_binding)) {
        return invalid(&h);
    }
    let Some(p) = decrypt::<Pending>(&s, row.org_id, b"oidc-pending", &row.payload_ciphertext)
    else {
        return invalid(&h);
    };
    let mut tx = match s.pool.begin().await {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    if sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(row.org_id)
        .fetch_optional(&mut *tx)
        .await
        .is_err()
    {
        return internal(&h);
    }
    let current = match flows::pending_in(&mut tx, &token.hash(), s.clock.now()).await {
        Ok(Some(v)) if v.org_id == row.org_id && v.config_version == row.config_version => v,
        _ => return invalid(&h),
    };
    if !bool::from(current.binding_hash.as_slice().ct_eq(&browser_binding)) {
        return invalid(&h);
    }
    let audit = crate::audit::ctx(&h, None);
    let identity = p.claims.identity();
    let resolution = match db::resolve_identity(
        &mut tx,
        row.org_id,
        row.config_version,
        &identity,
        None,
        s.clock.now(),
        &audit,
    )
    .await
    {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    let user = match resolution {
        db::Resolution::Resolved(u) => u,
        db::Resolution::LinkRequired(u) => {
            if password.is_empty() {
                return needed(&h, "SSO_LINK_REQUIRED");
            }
            let account = match craftcloud_db::accounts::get(&s.pool, u).await {
                Ok(Some(a)) => a,
                _ => return invalid(&h),
            };
            let reservation =
                match s.login_reserve(&account.normalized_email, ip.map(|v| v.0.0), &h) {
                    Ok(v) => v,
                    Err(r) => return *r,
                };
            let stored =
                match craftcloud_db::accounts::find_login(&s.pool, &account.normalized_email).await
                {
                    Ok(Some(a)) => a.password_hash,
                    _ => return invalid(&h),
                };
            let stored_copy = stored.clone();
            let mut proof = password.to_string();
            let verified = s
                .run_blocking(move || {
                    let ok = craftcloud_auth::verify_or_dummy(stored_copy.as_deref(), &proof);
                    proof.zeroize();
                    ok
                })
                .await
                .unwrap_or(false);
            if !verified {
                reservation.record_failure(s.now_secs());
                drop(tx);
                let mut failed = match s.pool.begin().await {
                    Ok(t) => t,
                    Err(_) => return internal(&h),
                };
                let _ = flows::attempted(&mut failed, &token.hash()).await;
                let _ = failed.commit().await;
                return needed(&h, "SSO_LINK_REQUIRED");
            }
            reservation.record_success();
            match db::resolve_identity(
                &mut tx,
                row.org_id,
                row.config_version,
                &identity,
                stored.as_deref(),
                s.clock.now(),
                &audit,
            )
            .await
            {
                Ok(db::Resolution::Resolved(u)) => u,
                _ => return invalid(&h),
            }
        }
        db::Resolution::SeatLimit => {
            return error_response(
                StatusCode::CONFLICT,
                "SEAT_LIMIT_REACHED",
                "The organization has no available seat.",
                &h,
            );
        }
        db::Resolution::Rejected => {
            return error_response(
                StatusCode::FORBIDDEN,
                "SSO_IDENTITY_REJECTED",
                "This company identity cannot access the organization.",
                &h,
            );
        }
    };
    let proof = if b.code.is_some() || b.recovery_code.is_some() {
        match factor(&s, user, b.code.as_deref(), b.recovery_code.as_deref()).await {
            Some(v) => Some(v),
            None => {
                drop(tx);
                let _ = two_factor::record_failed_proof(&s.pool, user, &audit).await;
                let mut failed = match s.pool.begin().await {
                    Ok(t) => t,
                    Err(_) => return internal(&h),
                };
                let _ = flows::attempted(&mut failed, &token.hash()).await;
                let _ = failed.commit().await;
                return needed(&h, "SSO_SECOND_FACTOR_REQUIRED");
            }
        }
    } else {
        None
    };
    let now = s.clock.now();
    let browser = match OpaqueToken::generate() {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    let access = match DeviceToken::generate(DeviceTokenKind::Access) {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    let refresh = match DeviceToken::generate(DeviceTokenKind::Refresh) {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    let access_expiry = now + s.settings.device_access_ttl;
    let refresh_expiry = now + s.settings.device_absolute_timeout;
    let session = match p.desktop.as_ref() {
        None => db::SessionInput::Browser {
            token_hash: browser.hash(),
            absolute_expires_at: now + s.settings.absolute_timeout,
        },
        Some(d) => db::SessionInput::Device(craftcloud_db::device_sessions::NewDeviceSession {
            user_id: user,
            name: d.device_name.clone(),
            client_kind: d.client_kind.clone(),
            client_id: d.client_id.clone(),
            known_app_ids: s
                .settings
                .app_catalog
                .iter()
                .map(|a| a.id.clone())
                .collect(),
            refresh_hash: refresh.hash(),
            access_hash: access.hash(),
            now,
            access_expires_at: access_expiry,
            idle_expires_at: (now + s.settings.device_idle_timeout).min(refresh_expiry),
            absolute_expires_at: refresh_expiry,
        }),
    };
    let outcome = match db::issue_session(
        &mut tx,
        row.org_id,
        user,
        row.config_version,
        p.claims.mfa,
        proof.as_ref().map(|v| &v.0),
        proof.as_ref().map(|v| v.1.as_slice()),
        &session,
        now,
        &audit,
        s.email_delivery_enabled(),
    )
    .await
    {
        Ok(v) => v,
        Err(_) => return internal(&h),
    };
    let id = match outcome {
        db::LoginOutcome::Created(id) => id,
        db::LoginOutcome::FactorRequired => return needed(&h, "SSO_SECOND_FACTOR_REQUIRED"),
        db::LoginOutcome::SetupRequired => {
            return error_response(
                StatusCode::FORBIDDEN,
                "SSO_TWO_FACTOR_SETUP_REQUIRED",
                "Ask the provider for MFA or use local recovery to set up a second factor.",
                &h,
            );
        }
        db::LoginOutcome::AppNotAssigned => {
            return error_response(
                StatusCode::FORBIDDEN,
                "APP_NOT_ASSIGNED",
                "This app is not assigned in the organization.",
                &h,
            );
        }
        db::LoginOutcome::Rejected => return invalid(&h),
    };
    if flows::consume_pending(&mut tx, &token.hash())
        .await
        .is_err()
    {
        return internal(&h);
    }
    let response = if let Some(d) = p.desktop {
        let code = match OpaqueToken::generate() {
            Ok(v) => v,
            Err(_) => return internal(&h),
        };
        let account: (String, String, Option<String>) = match sqlx::query_as(
            "SELECT normalized_email,display_name,instance_role FROM users WHERE id=$1",
        )
        .bind(user)
        .fetch_one(&mut *tx)
        .await
        {
            Ok(a) => a,
            Err(_) => return internal(&h),
        };
        let device_user = json!({"user_id":user,"email":account.0,"display_name":account.1,"instance_role":account.2});
        let grant = DeviceGrant {
            session_id: id,
            user_id: user,
            reply: json!({"access_token":access.expose(),"refresh_token":refresh.expose(),"access_expires_at":crate::orgs::rfc3339(access_expiry),"refresh_expires_at":crate::orgs::rfc3339(refresh_expiry),"device_session_id":id,"user":device_user}),
        };
        let Some(cipher) = encrypt(&s, row.org_id, b"oidc-device", &grant) else {
            return internal(&h);
        };
        let challenge = match URL_SAFE_NO_PAD
            .decode(d.code_challenge)
            .ok()
            .and_then(|v| <[u8; 32]>::try_from(v).ok())
        {
            Some(v) => v,
            None => return invalid(&h),
        };
        match flows::insert_pending(
            &mut tx,
            &code.hash(),
            row.org_id,
            row.config_version,
            &challenge,
            &cipher,
            now,
            60,
        )
        .await
        {
            Ok(true) => {}
            _ => return internal(&h),
        }
        let mut target = match url::Url::parse(&d.redirect_uri) {
            Ok(u) => u,
            Err(_) => return invalid(&h),
        };
        target
            .query_pairs_mut()
            .append_pair("code", code.expose())
            .append_pair("state", &d.state);
        Json(json!({"redirect_url":target.as_str()})).into_response()
    } else {
        let mut r = Json(json!({"completed":true})).into_response();
        if let Some(cookie) = crate::session::session_cookie(
            &s.settings,
            &browser,
            i64::try_from(s.settings.absolute_timeout.as_secs()).unwrap_or(i64::MAX),
        ) {
            r.headers_mut().append(header::SET_COOKIE, cookie);
        }
        r
    };
    if tx.commit().await.is_err() {
        return internal(&h);
    }
    set_binding(response, &s, "", 0)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Exchange {
    code: String,
    code_verifier: String,
}
pub(crate) async fn exchange(
    State(state): State<ApiState>,
    ip: Option<Extension<ClientIp>>,
    proxy: Option<Extension<crate::client_ip::TrustedProxy>>,
    h: HeaderMap,
    b: Result<Json<Exchange>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&h, "AUTH_UNAVAILABLE");
    };
    if !crate::device::tls_ready(&s, &h, proxy) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "TLS_REQUIRED",
            "Device sign-in requires TLS.",
            &h,
        );
    }
    if !origin_ok(&h, &s.settings) || h.contains_key(header::COOKIE) {
        return invalid(&h);
    }
    if let Err(r) = s.rate_limit_ip(crate::services::IpClass::Login, ip.map(|v| v.0.0), &h) {
        return *r;
    }
    let b = match b {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &h),
    };
    let verifier = Zeroizing::new(b.code_verifier);
    if verifier.len() < 43
        || verifier.len() > 128
        || !verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
    {
        return invalid(&h);
    }
    let Some(code) = OpaqueToken::parse(&b.code) else {
        return invalid(&h);
    };
    let row = match flows::pending(&s.pool, &code.hash(), s.clock.now()).await {
        Ok(Some(v)) => v,
        _ => return invalid(&h),
    };
    if !bool::from(row.binding_hash.as_slice().ct_eq(&hash(&verifier))) {
        return invalid(&h);
    }
    let Some(grant) =
        decrypt::<DeviceGrant>(&s, row.org_id, b"oidc-device", &row.payload_ciphertext)
    else {
        return invalid(&h);
    };
    let mut tx = match s.pool.begin().await {
        Ok(t) => t,
        Err(_) => return internal(&h),
    };
    if sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(row.org_id)
        .fetch_optional(&mut *tx)
        .await
        .is_err()
    {
        return internal(&h);
    }
    if !matches!(
        flows::pending_in(&mut tx, &code.hash(), s.clock.now()).await,
        Ok(Some(_))
    ) {
        return invalid(&h);
    }
    let valid:bool=match sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM device_sessions d JOIN users u ON u.id=d.user_id JOIN organization_members m ON m.user_id=u.id AND m.org_id=$3 JOIN organization_sso s ON s.org_id=m.org_id WHERE d.id=$1 AND d.user_id=$2 AND d.revoked_at IS NULL AND d.access_expires_at>$4 AND u.disabled_at IS NULL AND m.status='active' AND s.enabled AND s.version=$5 AND COALESCE((SELECT enabled FROM instance_sso_policy WHERE singleton),true) AND NOT EXISTS(SELECT 1 FROM organization_session_cutoffs c WHERE c.org_id=m.org_id AND c.user_id=u.id AND c.revoked_before>=d.created_at))").bind(grant.session_id).bind(grant.user_id).bind(row.org_id).bind(s.clock.now()).bind(row.config_version).fetch_one(&mut *tx).await{Ok(v)=>v,Err(_)=>return internal(&h)};
    if !valid {
        return invalid(&h);
    }
    if flows::consume_pending(&mut tx, &code.hash()).await.is_err() || tx.commit().await.is_err() {
        return internal(&h);
    }
    Json(grant.reply).into_response()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_redirects_require_literal_loopback_and_exact_path() {
        let mut d = Desktop {
            client_kind: "launcher".into(),
            client_id: "launcher".into(),
            device_name: "Test".into(),
            redirect_uri: "http://127.0.0.1:43121/oidc/callback".into(),
            code_challenge: URL_SAFE_NO_PAD.encode([1; 32]),
            state: "a".repeat(43),
        };
        assert!(desktop_valid(&d));
        for url in [
            "http://localhost:43121/oidc/callback",
            "http://127.0.0.1.evil.test:43121/oidc/callback",
            "http://127.0.0.2:43121/oidc/callback",
            "https://evil.test/oidc/callback",
            "http://127.0.0.1:43121/wrong",
            "http://a@127.0.0.1:43121/oidc/callback",
            "http://127.0.0.1:43121/oidc/callback?x=1",
        ] {
            d.redirect_uri = url.into();
            assert!(!desktop_valid(&d), "{url}");
        }
    }
}
