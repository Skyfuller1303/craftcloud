//! Desktop login, refresh and device-session management routes.

use axum::{
    Extension, Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{DeviceToken, DeviceTokenKind, hash_password, needs_rehash, verify_or_dummy};
use craftcloud_core::NormalizedEmail;
use craftcloud_db::{
    DbError, accounts,
    device_sessions::{self, NewDeviceSession, RefreshOutcome},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::audit::ctx as audit_ctx;
use crate::client_ip::TrustedProxy;
use crate::pagination::{PageQuery, decode_device_cursor, encode_device_cursor, is_device_ref};
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::services::{IpClass, Services};
use crate::session::{AuthenticatedSession, CredentialKind, ambiguous_credentials};
use crate::{ApiState, ClientIp, error_response};

const PASSWORD_INPUT_MAX_BYTES: usize = 1024;

/// Extension point consumed by AC-02's second-factor challenge flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondFactorLogin {
    /// Password accepted; no second factor is required.
    Complete,
    /// Password accepted; issue only this short-lived challenge.
    ChallengeRequired { challenge_id: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceLoginRequest {
    email: String,
    password: String,
    client_kind: String,
    client_id: String,
    device_name: String,
}

impl Drop for DeviceLoginRequest {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefreshRequest {
    refresh_token: String,
}

impl Drop for RefreshRequest {
    fn drop(&mut self) {
        self.refresh_token.zeroize();
    }
}

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

pub(crate) fn device_expiries(
    now: OffsetDateTime,
    access_ttl: std::time::Duration,
    idle_ttl: std::time::Duration,
    absolute_ttl: std::time::Duration,
) -> (OffsetDateTime, OffsetDateTime, OffsetDateTime) {
    let absolute = now + absolute_ttl;
    (
        (now + access_ttl).min(absolute),
        (now + idle_ttl).min(absolute),
        absolute,
    )
}

async fn second_factor_decision(
    services: &Services,
    user_id: Uuid,
    password_hash: &str,
    client_ip: Option<ClientIp>,
    client_kind: &str,
    client_id: &str,
    device_name: &str,
) -> Result<SecondFactorLogin, DbError> {
    Ok(crate::two_factor::issue_login_challenge(
        services,
        user_id,
        password_hash,
        "device",
        client_ip,
        Some((client_kind, client_id, device_name)),
    )
    .await?
    .map_or(SecondFactorLogin::Complete, |challenge_id| {
        SecondFactorLogin::ChallengeRequired { challenge_id }
    }))
}

fn db_failure(error: &DbError, headers: &HeaderMap, what: &str) -> Response {
    tracing::error!(%error, "{what}");
    match error {
        DbError::Connection(_) => unavailable(headers, "AUTH_UNAVAILABLE"),
        _ => internal(headers),
    }
}

fn invalid_login(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "INVALID_CREDENTIALS",
        "E-mail address or password is incorrect.",
        headers,
    )
}

pub(crate) fn tls_ready(
    services: &Services,
    headers: &HeaderMap,
    trusted_proxy: Option<Extension<TrustedProxy>>,
) -> bool {
    if services
        .settings
        .public_url
        .as_deref()
        .is_some_and(is_loopback_http_url)
    {
        return true;
    }
    if !trusted_proxy.is_some_and(|Extension(trusted)| trusted.0) {
        return false;
    }
    let mut values = headers.get_all("x-forwarded-proto").iter();
    matches!(
        (
            values.next().and_then(|value| value.to_str().ok()),
            values.next()
        ),
        (Some("https"), None)
    )
}

fn is_loopback_http_url(url: &str) -> bool {
    let Some(authority) = url.strip_prefix("http://") else {
        return false;
    };
    let host = authority.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
        || host
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn valid_client_kind(kind: &str) -> bool {
    matches!(kind, "app" | "launcher" | "cli")
}

fn clean_device_name(value: &str) -> Option<String> {
    let cleaned = value
        .chars()
        .filter(|ch| {
            !ch.is_control()
                && !matches!(*ch,
                '\u{00ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}' | '\u{feff}')
        })
        .collect::<String>();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned: String = cleaned.chars().take(64).collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn rfc_token(token: &DeviceToken) -> String {
    token.expose().to_owned()
}

pub(crate) fn token_reply(
    id: Uuid,
    access: &DeviceToken,
    refresh: &DeviceToken,
    access_expiry: OffsetDateTime,
    refresh_expiry: OffsetDateTime,
    user: Option<serde_json::Value>,
) -> Response {
    let mut body = json!({
        "access_token": rfc_token(access), "access_expires_at": rfc3339(access_expiry),
        "refresh_token": rfc_token(refresh), "refresh_expires_at": rfc3339(refresh_expiry),
        "device_session_id": id.to_string(),
    });
    if let (Some(user), Some(object)) = (user, body.as_object_mut()) {
        object.insert("user".to_owned(), user);
    }
    Json(body).into_response()
}

pub(crate) async fn login(
    State(state): State<ApiState>,
    client_ip: Option<Extension<ClientIp>>,
    trusted_proxy: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    body: Result<Json<DeviceLoginRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if ambiguous_credentials(&headers, &services.settings) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "AMBIGUOUS_CREDENTIALS",
            "Send either a session cookie or a bearer token, not both.",
            &headers,
        );
    }
    if !tls_ready(&services, &headers, trusted_proxy) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "DEVICE_TLS_REQUIRED",
            "Desktop sign-in requires HTTPS.",
            &headers,
        );
    }
    let ip = client_ip.map(|Extension(ip)| ip.0);
    let challenge_ip = ip.map(ClientIp);
    if let Err(response) = services.rate_limit_ip(IpClass::Login, ip, &headers) {
        return *response;
    }
    let request = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let email = NormalizedEmail::parse(&request.email).ok();
    let reservation = match &email {
        Some(email) => match services.login_reserve(email.as_str(), ip, &headers) {
            Ok(value) => Some(value),
            Err(response) => return *response,
        },
        None => None,
    };
    let record = match &email {
        Some(email) => match accounts::find_login(&services.pool, email.as_str()).await {
            Ok(record) => record,
            Err(error) => return db_failure(&error, &headers, "device login lookup failed"),
        },
        None => None,
    };
    let stored = record.as_ref().and_then(|row| row.password_hash.clone());
    let mut candidate = if request.password.len() > PASSWORD_INPUT_MAX_BYTES {
        String::new()
    } else {
        request.password.clone()
    };
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
    let user_id = record.as_ref().map(|row| row.user_id);
    let account = record.filter(|row| verified && !row.disabled);
    let (Some(account), Some(stored_hash)) = (account, stored) else {
        if let Some(reservation) = reservation {
            reservation.record_failure(services.now_secs());
        }
        tracing::info!("device login failed");
        crate::auth::record_login_failure(&services, &headers, user_id);
        return invalid_login(&headers);
    };
    if !valid_client_kind(&request.client_kind)
        || !is_client_id(&request.client_id)
        || request.device_name.len() > 512
        || clean_device_name(&request.device_name).is_none()
    {
        return validation_failed(&["client_kind", "client_id", "device_name"], &headers);
    }
    let cleaned_device_name = clean_device_name(&request.device_name).unwrap_or_default();
    let Ok(access) = DeviceToken::generate(DeviceTokenKind::Access) else {
        return internal(&headers);
    };
    let Ok(refresh) = DeviceToken::generate(DeviceTokenKind::Refresh) else {
        return internal(&headers);
    };
    match second_factor_decision(
        &services,
        account.user_id,
        &stored_hash,
        challenge_ip,
        &request.client_kind,
        &request.client_id,
        &cleaned_device_name,
    )
    .await
    {
        Err(error) => return db_failure(&error, &headers, "two-factor challenge issuance failed"),
        Ok(SecondFactorLogin::Complete) => {}
        Ok(SecondFactorLogin::ChallengeRequired { challenge_id }) => {
            drop(reservation);
            return crate::error_response_with_details(
                StatusCode::UNAUTHORIZED,
                "SECOND_FACTOR_REQUIRED",
                "A second factor is required.",
                &headers,
                Some(json!({ "challenge_id": challenge_id.to_string() })),
            );
        }
    }
    let now = services.clock.now();
    let (access_expiry, idle_expiry, absolute) = device_expiries(
        now,
        services.settings.device_access_ttl,
        services.settings.device_idle_timeout,
        services.settings.device_absolute_timeout,
    );
    let new = NewDeviceSession {
        user_id: account.user_id,
        name: cleaned_device_name.clone(),
        client_kind: request.client_kind.clone(),
        client_id: request.client_id.clone(),
        refresh_hash: refresh.hash(),
        access_hash: access.hash(),
        now,
        access_expires_at: access_expiry,
        idle_expires_at: idle_expiry,
        absolute_expires_at: absolute,
    };
    let created = device_sessions::create_for_verified_login_with_email(
        &services.pool,
        &new,
        &stored_hash,
        &audit_ctx(&headers, Some(account.user_id)),
        services.email_delivery_enabled(),
    )
    .await;
    let id = match created {
        Ok(device_sessions::LoginOutcome::Created(id)) => id,
        Ok(device_sessions::LoginOutcome::Rejected) => {
            match second_factor_decision(
                &services,
                account.user_id,
                &stored_hash,
                challenge_ip,
                &request.client_kind,
                &request.client_id,
                &cleaned_device_name,
            )
            .await
            {
                Ok(SecondFactorLogin::ChallengeRequired { challenge_id }) => {
                    drop(reservation);
                    return crate::error_response_with_details(
                        StatusCode::UNAUTHORIZED,
                        "SECOND_FACTOR_REQUIRED",
                        "A second factor is required.",
                        &headers,
                        Some(json!({"challenge_id":challenge_id})),
                    );
                }
                Ok(SecondFactorLogin::Complete) => {}
                Err(error) => {
                    return db_failure(&error, &headers, "two-factor challenge issuance failed");
                }
            }
            if let Some(reservation) = reservation {
                reservation.record_failure(services.now_secs());
            }
            return invalid_login(&headers);
        }
        Err(error) => return db_failure(&error, &headers, "device session creation failed"),
    };
    if let Some(reservation) = reservation {
        reservation.record_success();
    }
    crate::observe::note_actor(account.user_id);
    if needs_rehash(&stored_hash) {
        let mut candidate = request.password.clone();
        let new_hash = services
            .run_blocking(move || {
                let hash = hash_password(&candidate);
                candidate.zeroize();
                hash
            })
            .await
            .and_then(Result::ok);
        if let Some(new_hash) = new_hash {
            let _ =
                accounts::rehash_password(&services.pool, account.user_id, &stored_hash, &new_hash)
                    .await;
        }
    }
    let public = match accounts::get(&services.pool, account.user_id).await {
        Ok(Some(public)) => public,
        Ok(None) => return invalid_login(&headers),
        Err(error) => return db_failure(&error, &headers, "device login account lookup failed"),
    };
    let user = json!({ "user_id": public.id.to_string(), "email": public.normalized_email, "display_name": public.display_name, "instance_role": public.instance_role });
    token_reply(
        id,
        &access,
        &refresh,
        access_expiry,
        idle_expiry,
        Some(user),
    )
}

fn is_client_id(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) async fn refresh(
    State(state): State<ApiState>,
    client_ip: Option<Extension<ClientIp>>,
    headers: HeaderMap,
    trusted_proxy: Option<Extension<TrustedProxy>>,
    body: Result<Json<RefreshRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if ambiguous_credentials(&headers, &services.settings) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "AMBIGUOUS_CREDENTIALS",
            "Send either a session cookie or a bearer token, not both.",
            &headers,
        );
    }
    if !tls_ready(&services, &headers, trusted_proxy) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "DEVICE_TLS_REQUIRED",
            "Desktop sign-in requires HTTPS.",
            &headers,
        );
    }
    if let Err(response) = services.rate_limit_ip(
        IpClass::Login,
        client_ip.map(|Extension(ip)| ip.0),
        &headers,
    ) {
        return *response;
    }
    let Json(mut body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let Some(old) = DeviceToken::parse(&body.refresh_token, DeviceTokenKind::Refresh) else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
            "The refresh token is invalid.",
            &headers,
        );
    };
    let Ok(access) = DeviceToken::generate(DeviceTokenKind::Access) else {
        return internal(&headers);
    };
    let Ok(refresh) = DeviceToken::generate(DeviceTokenKind::Refresh) else {
        return internal(&headers);
    };
    let now = services.clock.now();
    let outcome = device_sessions::refresh(
        &services.pool,
        &old.hash(),
        &refresh.hash(),
        &access.hash(),
        now,
        services.settings.device_access_ttl,
        services.settings.device_idle_timeout,
        &audit_ctx(&headers, None),
    )
    .await;
    body.refresh_token.zeroize();
    match outcome {
        Ok((RefreshOutcome::Rotated, Some(row))) => token_reply(
            row.id,
            &access,
            &refresh,
            row.access_expires_at.unwrap_or(now),
            row.idle_expires_at,
            None,
        ),
        Ok(_) => error_response(
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
            "The refresh token is invalid.",
            &headers,
        ),
        Err(error) => db_failure(&error, &headers, "device refresh failed"),
    }
}

pub(crate) async fn logout(
    State(state): State<ApiState>,
    headers: HeaderMap,
    trusted_proxy: Option<Extension<TrustedProxy>>,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if !tls_ready(&services, &headers, trusted_proxy) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "DEVICE_TLS_REQUIRED",
            "Desktop sign-in requires HTTPS.",
            &headers,
        );
    }
    if session.credential_kind != CredentialKind::Device {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "A device bearer token is required.",
            &headers,
        );
    }
    match device_sessions::revoke_current(
        &services.pool,
        session.session_id,
        session.user_id,
        services.clock.now(),
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
        Err(error) => db_failure(&error, &headers, "device logout failed"),
    }
}

pub(crate) async fn list(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    match device_sessions::list_active(&services.pool, session.user_id, services.clock.now()).await {
        Ok(devices) => Json(json!({"devices": devices.iter().map(|d| json!({"id":d.id.to_string(),"name":d.name,"client_kind":d.client_kind,"client_id":d.client_id,"created_at":rfc3339(d.created_at),"last_used_at":rfc3339(d.last_used_at)})).collect::<Vec<_>>()})).into_response(),
        Err(error) => db_failure(&error, &headers, "device list failed"),
    }
}

pub(crate) async fn revoke(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(id): Path<String>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "The requested resource does not exist.",
            &headers,
        );
    };
    match device_sessions::revoke_owned(
        &services.pool,
        id,
        session.user_id,
        services.clock.now(),
        "user_revoked",
        &audit_ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "The requested resource does not exist.",
            &headers,
        ),
        Err(error) => db_failure(&error, &headers, "device revocation failed"),
    }
}

fn admin_device_ref(id: Uuid) -> String {
    let mut digest = Sha256::new();
    digest.update(b"craftcloud-admin-device-ref-v1\0");
    digest.update(id.as_bytes());
    let bytes = digest.finalize();
    let mut out = String::with_capacity(67);
    out.push_str("ds_");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(crate) async fn admin_list(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(user_raw): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let user_id = Uuid::parse_str(&user_raw).ok();
    if let Err(response) = crate::orgs::operator_only(
        &services,
        &session,
        craftcloud_core::policy::Action::UserDeviceList,
        "admin.user_devices.list",
        user_id,
        &headers,
    )
    .await
    {
        return *response;
    }
    let Ok(Query(query)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let limit = match query.limit() {
        Ok(limit) => limit,
        Err(error) => return crate::orgs::page_error(error, &headers),
    };
    let Some(user_id) = user_id else {
        return error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "The requested resource does not exist.",
            &headers,
        );
    };
    match accounts::get(&services.pool, user_id).await {
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                "The requested resource does not exist.",
                &headers,
            );
        }
        Err(error) => return db_failure(&error, &headers, "account lookup failed"),
        Ok(Some(_)) => {}
    }
    let scope = user_id.to_string();
    let after = match query
        .cursor()
        .map(|cursor| decode_device_cursor(cursor, &scope))
    {
        Some(Ok(position)) => Some(position),
        Some(Err(error)) => return crate::orgs::page_error(error, &headers),
        None => None,
    };
    let mut devices = match device_sessions::list_active_page(
        &services.pool,
        user_id,
        services.clock.now(),
        limit + 1,
        after,
    )
    .await
    {
        Ok(devices) => devices,
        Err(error) => return db_failure(&error, &headers, "device list failed"),
    };
    let has_more = devices.len() > limit as usize;
    if has_more {
        devices.truncate(limit as usize);
    }
    let next = devices
        .last()
        .filter(|_| has_more)
        .map(|last| encode_device_cursor(&scope, last.created_at, &admin_device_ref(last.id)));
    Json(json!({"devices":devices.iter().map(|device| json!({"device_ref":admin_device_ref(device.id),"name":device.name,"client_kind":device.client_kind,"client_id":device.client_id,"created_at":crate::orgs::rfc3339(device.created_at),"last_used_at":crate::orgs::rfc3339(device.last_used_at)})).collect::<Vec<_>>(),"next_cursor":next})).into_response()
}

pub(crate) async fn admin_revoke(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((user_raw, device_ref)): Path<(String, String)>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let user_id = Uuid::parse_str(&user_raw).ok();
    if let Err(response) = crate::orgs::operator_only(
        &services,
        &session,
        craftcloud_core::policy::Action::UserDeviceRevoke,
        "admin.user_device.revoke",
        user_id,
        &headers,
    )
    .await
    {
        return *response;
    }
    let Some(user_id) = user_id.filter(|_| is_device_ref(&device_ref)) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "The requested resource does not exist.",
            &headers,
        );
    };
    match accounts::get(&services.pool, user_id).await {
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                "The requested resource does not exist.",
                &headers,
            );
        }
        Err(error) => return db_failure(&error, &headers, "account lookup failed"),
        Ok(Some(_)) => {}
    }
    let device = match device_sessions::find_active_by_ref(
        &services.pool,
        user_id,
        services.clock.now(),
        &device_ref,
    )
    .await
    {
        Ok(device) => device,
        Err(error) => return db_failure(&error, &headers, "device lookup failed"),
    };
    let Some(device) = device else {
        return error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "The requested resource does not exist.",
            &headers,
        );
    };
    match device_sessions::revoke_admin_owned(
        &services.pool,
        device.id,
        user_id,
        session.user_id,
        services.clock.now(),
        &audit_ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(Some(_leases_removed)) => StatusCode::NO_CONTENT.into_response(),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "The requested resource does not exist.",
            &headers,
        ),
        Err(error) => db_failure(&error, &headers, "device revocation failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_expiry_never_exceeds_idle_or_absolute_expiry() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let (access, idle, absolute) = device_expiries(
            now,
            std::time::Duration::from_secs(900),
            std::time::Duration::from_secs(30 * 86400),
            std::time::Duration::from_secs(90 * 86400),
        );
        assert_eq!(access, now + time::Duration::minutes(15));
        assert_eq!(idle, now + time::Duration::days(30));
        assert_eq!(absolute, now + time::Duration::days(90));
        let (access, idle, absolute) = device_expiries(
            now,
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(60 * 86400),
            std::time::Duration::from_secs(12 * 3600),
        );
        assert!(access <= idle && idle <= absolute);
        assert_eq!(idle, absolute);
    }

    #[test]
    fn admin_device_cursor_contains_only_opaque_ref_and_timestamp() {
        use base64::Engine as _;

        let id = Uuid::new_v4();
        let reference = admin_device_ref(id);
        let cursor = encode_device_cursor("user-scope", OffsetDateTime::UNIX_EPOCH, &reference);
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&cursor);
        assert_eq!(
            decoded,
            Ok(format!("v1|admin-devices|user-scope|0|{reference}").into_bytes())
        );
        assert_eq!(
            decode_device_cursor(&cursor, "user-scope"),
            Ok((OffsetDateTime::UNIX_EPOCH, reference))
        );
    }
}
