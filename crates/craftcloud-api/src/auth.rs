//! Authentication routes (CC-F02): login, session info and device list,
//! logout, logout of all sessions, password change and password reset.
//!
//! Login answers a uniform 401 `INVALID_CREDENTIALS` for unknown e-mail,
//! wrong password, missing password hash and disabled account, and always
//! performs exactly one Argon2 verification (a dummy one for unknown
//! accounts) so timing does not reveal account existence. Login, setup and
//! reset routes are rate limited per client address (IPv4 exact, IPv6 /64);
//! login additionally counts **failed** attempts per account and per
//! account/address (see `ratelimit`). No credential, token or cookie value is
//! ever logged or returned in an error.

use axum::{
    Extension, Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{
    OpaqueToken, PASSWORD_RESET_TTL, hash_password, needs_rehash, validate_password,
    verify_or_dummy,
};
use craftcloud_core::{DisplayName, NormalizedEmail};
use craftcloud_db::audit::{AggScope, AuditResult, NewEvent};
use craftcloud_db::password_reset::IssueOutcome;
use craftcloud_db::{DbError, accounts, device_sessions, password_reset, sessions};
use serde::Deserialize;
use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::audit::{ctx as audit_ctx, record_lossy};
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::services::{IpClass, Services};
use crate::session::{
    AuthRejection, AuthenticatedSession, cleared_cookie, origin_ok, session_cookie,
    session_cookie_presented, session_token,
};
use crate::{ApiState, ClientIp, error_response};

/// Longest password considered at all; longer input never reaches Argon2.
const PASSWORD_INPUT_MAX_BYTES: usize = 1024;
/// Rows older than this past their expiry are deleted opportunistically.
const CLEANUP_GRACE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoginRequest {
    email: String,
    password: String,
}

impl Drop for LoginRequest {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

impl Drop for ChangePasswordRequest {
    fn drop(&mut self) {
        self.current_password.zeroize();
        self.new_password.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateProfileRequest {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResetPasswordRequest {
    token: String,
    new_password: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdminPasswordResetRequest {
    #[serde(default)]
    reset_two_factor: bool,
}

impl Drop for ResetPasswordRequest {
    fn drop(&mut self) {
        self.token.zeroize();
        self.new_password.zeroize();
    }
}

pub(crate) fn rfc3339(time: OffsetDateTime) -> String {
    time.format(&Rfc3339).unwrap_or_default()
}

fn db_failure(error: &DbError, headers: &HeaderMap, what: &str) -> Response {
    // DbError text is generic and free of request data.
    tracing::error!(%error, "{what}");
    match error {
        DbError::Connection(_) => unavailable(headers, "AUTH_UNAVAILABLE"),
        _ => internal(headers),
    }
}

pub(crate) fn invalid_credentials(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "INVALID_CREDENTIALS",
        "E-mail address or password is incorrect.",
        headers,
    )
}

fn csrf_failed(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "CSRF_FAILED",
        "The request failed the cross-site request forgery check.",
        headers,
    )
}

fn not_found(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "The requested resource does not exist.",
        headers,
    )
}

pub(crate) fn with_cookie(
    mut response: Response,
    cookie: Option<axum::http::HeaderValue>,
) -> Response {
    if let Some(cookie) = cookie {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}

/// Removes long-expired rows; failures are ignored (best effort). Runs at
/// most once per ten minutes per process.
pub(crate) async fn cleanup(services: &Services) {
    if !services.cleanup_due() {
        return;
    }
    let cutoff = services.clock.now() - CLEANUP_GRACE;
    let _ = sessions::delete_expired(&services.pool, cutoff).await;
    let _ = password_reset::delete_expired(&services.pool, cutoff).await;
    let _ =
        craftcloud_db::two_factor::delete_expired_challenges(&services.pool, services.clock.now())
            .await;
}

fn client_ip_of(ip: Option<Extension<ClientIp>>) -> Option<std::net::IpAddr> {
    ip.map(|Extension(ip)| ip.0)
}

/// Bounds password input before hashing; oversized input is treated as an
/// empty (certainly wrong) password.
fn bounded(password: &str) -> String {
    if password.len() > PASSWORD_INPUT_MAX_BYTES {
        String::new()
    } else {
        password.to_owned()
    }
}

/// Turns an authentication failure into a response; a 401 additionally
/// clears the session cookie so the browser drops the dead cookie.
fn rejected(rejection: AuthRejection, services: &Services, headers: &HeaderMap) -> Response {
    let unauthenticated = rejection.is_unauthenticated();
    let response = rejection.into_response();
    // Only a request that actually presented a session cookie gets it cleared.
    if unauthenticated && session_cookie_presented(headers, &services.settings) {
        with_cookie(response, cleared_cookie(&services.settings))
    } else {
        response
    }
}

// ---- POST /auth/login ------------------------------------------------------

/// Records a failed login as an aggregated audit row: one row per attacked
/// account and 15-minute window with a counter (all unknown addresses share one
/// row, so the log never holds an e-mail address). Nobody is authenticated, so
/// the row has no actor; the account is the target. Best-effort, in a task of
/// its own: the answer to the client does not depend on it.
pub(crate) fn record_login_failure(
    services: &Services,
    headers: &HeaderMap,
    user_id: Option<Uuid>,
) {
    let mut event = NewEvent::new("auth.login").result(AuditResult::Failed);
    if let Some(user_id) = user_id {
        event = event.target("user", user_id);
    }
    record_lossy(
        services,
        &audit_ctx(headers, None),
        &event,
        Some(AggScope::Target),
    );
}

pub(crate) fn record_two_factor_failure(services: &Services, headers: &HeaderMap, user_id: Uuid) {
    let event = NewEvent::new("auth.two_factor_failed")
        .result(AuditResult::Failed)
        .target("user", user_id);
    record_lossy(
        services,
        &audit_ctx(headers, None),
        &event,
        Some(AggScope::Target),
    );
}

pub(crate) async fn login(
    State(state): State<ApiState>,
    client_ip: Option<Extension<ClientIp>>,
    headers: HeaderMap,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let ip = client_ip_of(client_ip);
    if let Err(response) = services.rate_limit_ip(IpClass::Login, ip, &headers) {
        return *response;
    }
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    // Login CSRF: the Origin check only (there is no session token yet).
    if !origin_ok(&headers, &services.settings) {
        return csrf_failed(&headers);
    }

    let email = NormalizedEmail::parse(&request.email).ok();
    // Failed attempts are counted per normalized e-mail address, for existing
    // and unknown addresses alike, so the pattern reveals nothing. The attempt
    // is reserved atomically *before* the lookup and the expensive
    // verification, so concurrent attempts cannot all slip past the limit.
    let reservation = match &email {
        Some(email) => match services.login_reserve(email.as_str(), ip, &headers) {
            Ok(reservation) => Some(reservation),
            Err(response) => return *response,
        },
        None => None,
    };
    let record = match &email {
        Some(email) => match accounts::find_login(&services.pool, email.as_str()).await {
            Ok(record) => record,
            Err(error) => return db_failure(&error, &headers, "login lookup failed"),
        },
        None => None,
    };

    // Exactly one verification, whether or not the account exists.
    let stored = record.as_ref().and_then(|r| r.password_hash.clone());
    let mut candidate = bounded(&request.password);
    let verify_stored = stored.clone();
    let verified = services
        .run_blocking(move || {
            let ok = verify_or_dummy(verify_stored.as_deref(), &candidate);
            candidate.zeroize();
            ok
        })
        .await;
    let Some(verified) = verified else {
        tracing::error!("password verification could not run");
        return internal(&headers);
    };

    let known_user = record.as_ref().map(|r| r.user_id);
    let account = record.filter(|r| verified && !r.disabled);
    let (Some(account), Some(stored_hash)) = (account, stored) else {
        if let Some(reservation) = reservation {
            reservation.record_failure(services.now_secs());
        }
        tracing::info!("login failed");
        record_login_failure(&services, &headers, known_user);
        return invalid_credentials(&headers);
    };

    match crate::two_factor::issue_login_challenge(
        &services,
        account.user_id,
        &stored_hash,
        "browser",
        ip.map(ClientIp),
        None,
    )
    .await
    {
        Ok(Some(challenge_id)) => {
            drop(reservation);
            return crate::error_response_with_details(
                StatusCode::UNAUTHORIZED,
                "SECOND_FACTOR_REQUIRED",
                "A second factor is required.",
                &headers,
                Some(json!({ "challenge_id": challenge_id.to_string() })),
            );
        }
        Ok(None) => {}
        Err(error) => return db_failure(&error, &headers, "two-factor challenge issuance failed"),
    }

    // The session is only created if the account still has exactly the
    // verified password hash and is enabled at the moment of the insert; a
    // reset or password change that committed meanwhile makes this fail.
    let Ok(token) = OpaqueToken::generate() else {
        tracing::error!("session token generation failed");
        return internal(&headers);
    };
    let now = services.clock.now();
    let absolute = now + services.settings.absolute_timeout;
    let created = sessions::create_for_verified_login_with_email(
        &services.pool,
        &sessions::NewSession {
            user_id: account.user_id,
            token_hash: token.hash(),
            now,
            absolute_expires_at: absolute,
        },
        &stored_hash,
        &audit_ctx(&headers, Some(account.user_id)),
        services.email_delivery_enabled(),
    )
    .await;
    match created {
        Ok(sessions::LoginOutcome::Created(_)) => {}
        Ok(sessions::LoginOutcome::EmailRateLimited) => {
            return crate::error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "Try again later.",
                &headers,
            );
        }
        Ok(sessions::LoginOutcome::Rejected) => {
            match crate::two_factor::issue_login_challenge(
                &services,
                account.user_id,
                &stored_hash,
                "browser",
                ip.map(ClientIp),
                None,
            )
            .await
            {
                Ok(Some(challenge_id)) => {
                    drop(reservation);
                    return crate::error_response_with_details(
                        StatusCode::UNAUTHORIZED,
                        "SECOND_FACTOR_REQUIRED",
                        "A second factor is required.",
                        &headers,
                        Some(json!({"challenge_id": challenge_id})),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    return db_failure(&error, &headers, "two-factor challenge issuance failed");
                }
            }
            if let Some(reservation) = reservation {
                reservation.record_failure(services.now_secs());
            }
            tracing::info!("login failed");
            record_login_failure(&services, &headers, Some(account.user_id));
            return invalid_credentials(&headers);
        }
        Err(error) => return db_failure(&error, &headers, "session creation failed"),
    }
    if let Some(reservation) = reservation {
        reservation.record_success();
    }
    crate::observe::note_actor(account.user_id);

    // Upgrade the stored hash if it uses outdated parameters (compare and
    // set on the verified hash, after the session insert that depends on it).
    if needs_rehash(&stored_hash) {
        let mut candidate = bounded(&request.password);
        let new_hash = services
            .run_blocking(move || {
                let hash = hash_password(&candidate);
                candidate.zeroize();
                hash
            })
            .await
            .and_then(Result::ok);
        if let Some(new_hash) = new_hash {
            match accounts::rehash_password(
                &services.pool,
                account.user_id,
                &stored_hash,
                &new_hash,
            )
            .await
            {
                Ok(_) => tracing::info!("password hash upgraded"),
                Err(error) => tracing::warn!(%error, "password hash upgrade failed"),
            }
        }
    }

    // A login with a still valid session cookie replaces that session.
    if let Some(old) = session_token(&headers, &services.settings) {
        let _ = sessions::revoke_by_token_hash(&services.pool, &old.hash(), now).await;
    }
    cleanup(&services).await;

    let expires_at = (now + services.settings.idle_timeout).min(absolute);
    let max_age = (absolute - now).whole_seconds();
    tracing::info!("login succeeded");
    with_cookie(
        Json(json!({
            "user_id": account.user_id.to_string(),
            "csrf_token": token.csrf_token(),
            "expires_at": rfc3339(expires_at),
        }))
        .into_response(),
        session_cookie(&services.settings, &token, max_age),
    )
}

// ---- GET /auth/session -----------------------------------------------------

pub(crate) async fn session(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    session_response(&services, &headers, &session).await
}

async fn session_response(
    services: &Services,
    headers: &HeaderMap,
    session: &AuthenticatedSession,
) -> Response {
    let account = match accounts::get(&services.pool, session.user_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return invalid_credentials(headers),
        Err(error) => return db_failure(&error, headers, "account lookup failed"),
    };
    Json(json!({
        "user_id": account.id.to_string(),
        "email": account.normalized_email,
        "email_verified": account.email_verified_at.is_some(),
        "display_name": account.display_name,
        "instance_role": account.instance_role,
        "csrf_token": session.csrf_token(),
        "created_at": rfc3339(session.created_at),
        "expires_at": rfc3339(session.expires_at),
    }))
    .into_response()
}

// ---- PATCH /auth/profile ---------------------------------------------------

/// Changes the display name of the calling account (cookie session with CSRF
/// token or device bearer). Only the caller's own account is addressed, so no
/// resource policy applies; the e-mail address stays operator-managed. The
/// audit event `auth.profile_updated` never contains the name.
pub(crate) async fn update_profile(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    body: Result<Json<UpdateProfileRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let Some(Ok(name)) = request.display_name.as_deref().map(DisplayName::parse) else {
        return validation_failed(&["display_name"], &headers);
    };
    if let Err(response) = services.rate_limit_profile(session.user_id, &headers) {
        return *response;
    }
    // An unchanged name writes nothing and records no audit event.
    match accounts::get(&services.pool, session.user_id).await {
        Ok(Some(account)) if account.display_name == name.as_str() => {
            return session_response(&services, &headers, &session).await;
        }
        Ok(Some(_)) => {}
        Ok(None) => return invalid_credentials(&headers),
        Err(error) => return db_failure(&error, &headers, "account lookup failed"),
    }
    match accounts::update_display_name_audited(
        &services.pool,
        session.user_id,
        name.as_str(),
        &audit_ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return invalid_credentials(&headers),
        Err(error) => return db_failure(&error, &headers, "profile update failed"),
    }
    session_response(&services, &headers, &session).await
}

// ---- GET /auth/sessions, DELETE /auth/sessions/{id} -----------------------

pub(crate) async fn list_sessions(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let now = services.clock.now();
    let idle = services.settings.idle_timeout;
    match sessions::list_active_for_user(&services.pool, session.user_id, now, idle).await {
        Ok(rows) => {
            let items: Vec<_> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id": row.id.to_string(),
                        "created_at": rfc3339(row.created_at),
                        "last_seen_at": rfc3339(row.last_seen_at),
                        "expires_at": rfc3339((row.last_seen_at + idle).min(row.absolute_expires_at)),
                        "current": row.id == session.session_id,
                    })
                })
                .collect();
            Json(json!({ "sessions": items })).into_response()
        }
        Err(error) => db_failure(&error, &headers, "session list failed"),
    }
}

pub(crate) async fn revoke_session(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(target): Path<String>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Ok(target_id) = Uuid::parse_str(&target) else {
        return not_found(&headers);
    };
    let now = services.clock.now();
    // Ending another session of the account is an audit event; ending the
    // one in use (a logout) is not.
    match sessions::revoke_owned_audited(
        &services.pool,
        target_id,
        session.user_id,
        now,
        &audit_ctx(&headers, Some(session.user_id)),
        target_id != session.session_id,
    )
    .await
    {
        // Another user's, unknown and already revoked sessions look the same.
        Ok(false) => not_found(&headers),
        Ok(true) if target_id == session.session_id => with_cookie(
            StatusCode::NO_CONTENT.into_response(),
            cleared_cookie(&services.settings),
        ),
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => db_failure(&error, &headers, "session revoke failed"),
    }
}

// ---- POST /auth/logout, /auth/logout-all ----------------------------------

pub(crate) async fn logout(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: Result<AuthenticatedSession, AuthRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let session = match session {
        Ok(session) => session,
        Err(rejection) => return rejected(rejection, &services, &headers),
    };
    let now = services.clock.now();
    if session.credential_kind == crate::session::CredentialKind::Device {
        match device_sessions::revoke_current(
            &services.pool,
            session.session_id,
            session.user_id,
            now,
            &audit_ctx(&headers, Some(session.user_id)),
        )
        .await
        {
            Ok(true) => StatusCode::NO_CONTENT.into_response(),
            Ok(false) => error_response(
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
                "Authentication is required.",
                &headers,
            ),
            Err(error) => db_failure(&error, &headers, "logout failed"),
        }
    } else {
        if let Err(error) = sessions::revoke(&services.pool, session.session_id, now).await {
            return db_failure(&error, &headers, "logout failed");
        }
        with_cookie(
            StatusCode::NO_CONTENT.into_response(),
            cleared_cookie(&services.settings),
        )
    }
}

pub(crate) async fn logout_all(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: Result<AuthenticatedSession, AuthRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let session = match session {
        Ok(session) => session,
        Err(rejection) => return rejected(rejection, &services, &headers),
    };
    let now = services.clock.now();
    if let Err(error) = sessions::revoke_all_for_user_audited(
        &services.pool,
        session.user_id,
        now,
        &audit_ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        return db_failure(&error, &headers, "logout-all failed");
    }
    with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        cleared_cookie(&services.settings),
    )
}

// ---- POST /auth/password/change -------------------------------------------

pub(crate) async fn change_password(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    body: Result<Json<ChangePasswordRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    // Guessing the current password through a stolen session is rate limited.
    if let Err(response) = services.rate_limit_pwchange(session.user_id, &headers) {
        return *response;
    }

    let account = match accounts::get(&services.pool, session.user_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return invalid_credentials(&headers),
        Err(error) => return db_failure(&error, &headers, "account lookup failed"),
    };
    let stored = match accounts::password_hash(&services.pool, session.user_id).await {
        Ok(stored) => stored,
        Err(error) => return db_failure(&error, &headers, "password lookup failed"),
    };
    let verify_stored = stored.clone();
    let mut candidate = bounded(&request.current_password);
    let verified = services
        .run_blocking(move || {
            let ok = verify_or_dummy(verify_stored.as_deref(), &candidate);
            candidate.zeroize();
            ok
        })
        .await;
    match verified {
        Some(true) => {}
        Some(false) => {
            return error_response(
                StatusCode::FORBIDDEN,
                "INVALID_CURRENT_PASSWORD",
                "The current password is incorrect.",
                &headers,
            );
        }
        None => return internal(&headers),
    }

    let local = account
        .normalized_email
        .split_once('@')
        .map_or("", |(local, _)| local);
    if validate_password(&request.new_password, local).is_err() {
        return validation_failed(&["new_password"], &headers);
    }
    let mut new_password = request.new_password.clone();
    let hashed = services
        .run_blocking(move || {
            let hash = hash_password(&new_password);
            new_password.zeroize();
            hash
        })
        .await
        .and_then(Result::ok);
    let Some(new_hash) = hashed else {
        tracing::error!("password hashing failed");
        return internal(&headers);
    };

    let Some(verified_hash) = stored else {
        return internal(&headers);
    };
    let now = services.clock.now();
    let changed = if session.credential_kind == crate::session::CredentialKind::Device {
        accounts::change_password_device_audited_with_email(
            &services.pool,
            session.user_id,
            &verified_hash,
            &new_hash,
            session.session_id,
            now,
            &audit_ctx(&headers, Some(session.user_id)),
            services.email_delivery_enabled(),
        )
        .await
    } else {
        accounts::change_password_audited_with_email(
            &services.pool,
            session.user_id,
            &verified_hash,
            &new_hash,
            session.session_id,
            now,
            &audit_ctx(&headers, Some(session.user_id)),
            services.email_delivery_enabled(),
        )
        .await
    };
    match changed {
        Ok(accounts::PasswordChangeResult::Changed(_)) => {
            tracing::info!("password changed; other sessions and pending reset links revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        // The password was changed or reset, or this session was revoked,
        // while the request was being processed: the session is gone (401).
        Ok(accounts::PasswordChangeResult::EmailRateLimited) => crate::error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "Try again later.",
            &headers,
        ),
        Ok(accounts::PasswordChangeResult::NotChanged) => {
            let response = error_response(
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
                "Authentication is required.",
                &headers,
            );
            if session.credential_kind == crate::session::CredentialKind::Cookie {
                with_cookie(response, cleared_cookie(&services.settings))
            } else {
                response
            }
        }
        Err(error) => db_failure(&error, &headers, "password change failed"),
    }
}

// ---- /admin/users/{user_id}/password-reset --------------------------------

/// Common checks for the operator-only reset routes: the policy engine decides
/// whether the actor may manage reset links (operator action), then the target
/// account must exist. Returns the target account id.
async fn operator_target(
    services: &Services,
    session: &AuthenticatedSession,
    action: craftcloud_core::policy::Action,
    action_name: &str,
    target: &str,
    headers: &HeaderMap,
) -> Result<Uuid, Box<Response>> {
    let target_id = Uuid::parse_str(target).ok();
    crate::orgs::operator_only(services, session, action, action_name, target_id, headers).await?;
    let Some(user_id) = target_id else {
        return Err(Box::new(not_found(headers)));
    };
    match accounts::get(&services.pool, user_id).await {
        Ok(Some(_)) => Ok(user_id),
        Ok(None) => Err(Box::new(not_found(headers))),
        Err(error) => Err(Box::new(db_failure(
            &error,
            headers,
            "account lookup failed",
        ))),
    }
}

pub(crate) async fn issue_password_reset(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(target): Path<String>,
    body: Result<Option<Json<AdminPasswordResetRequest>>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let user_id = match operator_target(
        &services,
        &session,
        craftcloud_core::policy::Action::PasswordResetIssue,
        "password_reset.issue",
        &target,
        &headers,
    )
    .await
    {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let reset_two_factor = match body {
        Ok(Some(Json(body))) => body.reset_two_factor,
        Ok(None) => false,
        Err(error) => return rejection_response(&error, &headers),
    };
    let Some(public_url) = services.settings.public_url.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "AUTH_UNAVAILABLE",
            "CRAFTCLOUD_PUBLIC_URL must be configured to build reset links.",
            &headers,
        );
    };

    let Ok(token) = OpaqueToken::generate() else {
        tracing::error!("reset token generation failed");
        return internal(&headers);
    };
    let now = services.clock.now();
    let expires_at = now + PASSWORD_RESET_TTL;
    let new = password_reset::NewResetToken {
        user_id,
        token_hash: token.hash(),
        created_by: Some(session.user_id),
        reset_two_factor,
        now,
        expires_at,
    };
    let email_draft = if services.email_delivery_enabled() {
        let account = match accounts::get(&services.pool, user_id).await {
            Ok(Some(account)) => account,
            Ok(None) => return not_found(&headers),
            Err(error) => return db_failure(&error, &headers, "account lookup failed"),
        };
        let id = Uuid::new_v4();
        let encrypted_token = match services.encrypt_email_token(id, token.expose().as_bytes()) {
            Ok(value) => value,
            Err(_) => return unavailable(&headers, "TWO_FACTOR_UNAVAILABLE"),
        };
        Some(craftcloud_db::email_outbox::EmailDraft {
            id,
            recipient: account.normalized_email,
            template: "password_reset".to_owned(),
            language: "en".to_owned(),
            params: json!({"recipient_name": account.display_name}),
            encrypted_token: Some(encrypted_token),
            expires_at,
            actor_id: Some(session.user_id),
            is_resend: false,
            now,
        })
    } else {
        None
    };
    let audit = audit_ctx(&headers, Some(session.user_id));
    let outcome = if let Some(draft) = email_draft.as_ref() {
        password_reset::create_audited_with_email(&services.pool, &new, &audit, draft).await
    } else {
        password_reset::create_audited(&services.pool, &new, &audit).await
    };
    match outcome {
        Ok(IssueOutcome::Issued { .. }) => {}
        Ok(IssueOutcome::AccountDisabled) => {
            return error_response(
                StatusCode::CONFLICT,
                "ACCOUNT_DISABLED",
                "The account is disabled; no reset link can be issued.",
                &headers,
            );
        }
        Ok(IssueOutcome::NoSuchUser) => return not_found(&headers),
        Ok(IssueOutcome::EmailRateLimited) => {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "Too many email messages were requested. Try again later.",
                &headers,
            );
        }
        Err(error) => return db_failure(&error, &headers, "reset token creation failed"),
    }
    tracing::info!("password reset link issued; sessions of the account revoked");
    if services.email_delivery_enabled() {
        let recipient = email_draft
            .as_ref()
            .map(|draft| draft.recipient.as_str())
            .unwrap_or_default();
        return (
            StatusCode::ACCEPTED,
            Json(json!({ "sent_to": recipient, "expires_at": rfc3339(expires_at) })),
        )
            .into_response();
    }
    // The token travels in the URL fragment so it is not sent to servers,
    // proxies or Referer headers.
    let reset_url = format!("{public_url}/reset-password#token={}", token.expose());
    (
        StatusCode::CREATED,
        Json(json!({ "reset_url": reset_url, "expires_at": rfc3339(expires_at) })),
    )
        .into_response()
}

pub(crate) async fn void_password_reset(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(target): Path<String>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let user_id = match operator_target(
        &services,
        &session,
        craftcloud_core::policy::Action::PasswordResetVoid,
        "password_reset.void",
        &target,
        &headers,
    )
    .await
    {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match password_reset::void_pending_audited(
        &services.pool,
        user_id,
        services.clock.now(),
        &audit_ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(_) => {
            tracing::info!("pending password reset links voided");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => db_failure(&error, &headers, "voiding reset links failed"),
    }
}

// ---- POST /auth/password-reset --------------------------------------------

pub(crate) async fn reset_password(
    State(state): State<ApiState>,
    client_ip: Option<Extension<ClientIp>>,
    headers: HeaderMap,
    body: Result<Json<ResetPasswordRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = services.rate_limit_ip(IpClass::Reset, client_ip_of(client_ip), &headers)
    {
        return *response;
    }
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let invalid_token = || {
        error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_RESET_TOKEN",
            "The reset link is invalid, expired or already used.",
            &headers,
        )
    };

    let Some(token) = OpaqueToken::parse(request.token.trim()) else {
        return invalid_token();
    };
    let now = services.clock.now();
    let target = match password_reset::peek(&services.pool, &token.hash(), now).await {
        Ok(Some(target)) => target,
        Ok(None) => return invalid_token(),
        Err(error) => return db_failure(&error, &headers, "reset token lookup failed"),
    };

    // Only a holder of a valid token learns about password policy problems.
    let local = target
        .normalized_email
        .split_once('@')
        .map_or("", |(local, _)| local);
    if validate_password(&request.new_password, local).is_err() {
        return validation_failed(&["new_password"], &headers);
    }
    let mut new_password = request.new_password.clone();
    let hashed = services
        .run_blocking(move || {
            let hash = hash_password(&new_password);
            new_password.zeroize();
            hash
        })
        .await
        .and_then(Result::ok);
    let Some(new_hash) = hashed else {
        tracing::error!("password hashing failed");
        return internal(&headers);
    };

    match password_reset::consume_audited_with_email(
        &services.pool,
        &token.hash(),
        &new_hash,
        services.clock.now(),
        &audit_ctx(&headers, None),
        services.email_delivery_enabled(),
    )
    .await
    {
        Ok(password_reset::ConsumeOutcome::Consumed(_)) => {
            tracing::info!("password reset completed; all sessions revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        // Consumed, expired or the account got disabled in the meantime.
        Ok(password_reset::ConsumeOutcome::EmailRateLimited) => crate::error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "Try again later.",
            &headers,
        ),
        Ok(password_reset::ConsumeOutcome::InvalidToken) => invalid_token(),
        Err(error) => db_failure(&error, &headers, "password reset failed"),
    }
}
