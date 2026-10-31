//! Shared helpers for issuing AC-02 login challenges.

use axum::{
    Extension, Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{
    DeviceToken, DeviceTokenKind, OpaqueToken, base32_encode, decrypt_secret_for,
    encrypt_secret_for, generate_totp_secret, hash_recovery_code, recovery_code_hash,
    recovery_codes, verify_or_dummy, verify_totp,
};
use craftcloud_db::{DbError, accounts, device_sessions::NewDeviceSession, sessions, two_factor};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::services::Services;
use crate::session::AuthenticatedSession;
use crate::{ApiState, ClientIp, error_response};

fn session_kind(session: &AuthenticatedSession) -> &'static str {
    match session.credential_kind {
        crate::session::CredentialKind::Device => "device",
        crate::session::CredentialKind::Cookie => "browser",
    }
}

pub(crate) fn management_attempt(
    services: &Services,
    session: &AuthenticatedSession,
    ip: Option<ClientIp>,
    headers: &HeaderMap,
) -> Result<crate::ratelimit::Reservation, Box<Response>> {
    services.two_factor_management_reserve(session.user_id, ip.map(|value| value.0), headers)
}

fn management_failure(
    services: &Services,
    reservation: crate::ratelimit::Reservation,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
) -> Response {
    reservation.record_failure(services.now_secs());
    crate::auth::record_two_factor_failure(services, headers, session.user_id);
    invalid(headers)
}

/// A wrong second-factor proof on a management route: counts toward the persistent
/// per-account cap in addition to the management budget.
pub(crate) async fn proof_failure(
    services: &Services,
    reservation: crate::ratelimit::Reservation,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
) -> Response {
    if let Err(error) = two_factor::record_failed_proof(
        &services.pool,
        session.user_id,
        &crate::audit::ctx(headers, Some(session.user_id)),
    )
    .await
    {
        return failed(&error, headers);
    }
    management_failure(services, reservation, session, headers)
}
use crate::client_ip::TrustedProxy;

pub(crate) async fn issue_login_challenge(
    services: &Services,
    user_id: Uuid,
    password_hash: &str,
    login_kind: &str,
    client_ip: Option<ClientIp>,
    device: Option<(&str, &str, &str)>,
) -> Result<Option<String>, craftcloud_db::DbError> {
    crate::auth::cleanup(services).await;
    let active = two_factor::secret(&services.pool, user_id)
        .await?
        .is_some_and(|record| record.enabled_at.is_some());
    if !active {
        return Ok(None);
    }
    let challenge_token =
        OpaqueToken::generate().map_err(|_| craftcloud_db::DbError::InvalidUrl)?;
    let challenge_id = challenge_token.expose().to_owned();
    let id_hash = challenge_token.hash();
    let password_hash_digest: [u8; 32] = Sha256::digest(password_hash.as_bytes()).into();
    let address = client_ip
        .map(|ip| ip.0.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let (kind, client_id, device_name) = device
        .map(|(kind, id, name)| (Some(kind), Some(id), Some(name)))
        .unwrap_or((None, None, None));
    let inserted = two_factor::create_challenge(
        &services.pool,
        &id_hash,
        user_id,
        &password_hash_digest,
        login_kind,
        &address,
        kind,
        client_id,
        device_name,
        services.clock.now(),
    )
    .await?;
    Ok(inserted.then_some(challenge_id))
}

pub(crate) fn challenge_hash(id: &str) -> Option<[u8; 32]> {
    OpaqueToken::parse(id).map(|token| token.hash())
}

fn failed(error: &DbError, headers: &HeaderMap) -> Response {
    tracing::error!(%error, "two-factor database operation failed");
    match error {
        DbError::Connection(_) => unavailable(headers, "AUTH_UNAVAILABLE"),
        _ => internal(headers),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PasswordBody {
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfirmBody {
    code: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisableBody {
    password: String,
    code: Option<String>,
    recovery_code: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChallengeBody {
    challenge_id: String,
    code: Option<String>,
    recovery_code: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryBody {
    code: Option<String>,
    recovery_code: Option<String>,
}

pub(crate) async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let (enabled, recovery_codes_remaining) =
        match two_factor::status(&services.pool, session.user_id).await {
            Ok(value) => value,
            Err(error) => return failed(&error, &headers),
        };
    Json(json!({"enabled": enabled, "available": services.two_factor_available(), "recovery_codes_remaining": recovery_codes_remaining})).into_response()
}

pub(crate) async fn setup(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    session: AuthenticatedSession,
    body: Result<Json<PasswordBody>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(key) = services.two_factor_keys.current.as_ref() else {
        return unavailable(&headers, "TWO_FACTOR_UNAVAILABLE");
    };
    let reservation = match management_attempt(&services, &session, ip.map(|v| v.0), &headers) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let Json(mut body) = match body {
        Ok(value) => value,
        Err(error) => return rejection_response(&error, &headers),
    };
    if body.password.len() > 1024 {
        body.password.zeroize();
        return validation_failed(&["password"], &headers);
    }
    let account = match accounts::get(&services.pool, session.user_id).await {
        Ok(Some(a)) => a,
        Ok(None) => return invalid(&headers),
        Err(error) => return failed(&error, &headers),
    };
    let login = match accounts::find_login(&services.pool, &account.normalized_email).await {
        Ok(Some(value)) => value,
        Ok(None) => return invalid(&headers),
        Err(error) => return failed(&error, &headers),
    };
    let password_hash = login.password_hash;
    let verify_hash = password_hash.clone();
    let mut password = body.password;
    let verified = services
        .run_blocking(move || {
            let ok = verify_or_dummy(verify_hash.as_deref(), &password);
            password.zeroize();
            ok
        })
        .await
        .unwrap_or(false);
    if !verified {
        return management_failure(&services, reservation, &session, &headers);
    }
    let secret = match generate_totp_secret() {
        Ok(value) => value,
        Err(_) => return internal(&headers),
    };
    let ciphertext = match encrypt_secret_for(key, session.user_id, b"totp", &secret) {
        Ok(value) => value,
        Err(_) => return internal(&headers),
    };
    let Some(password_hash) = password_hash else {
        return invalid(&headers);
    };
    match two_factor::begin_setup(
        &services.pool,
        session.user_id,
        session.session_id,
        session_kind(&session),
        &password_hash,
        &ciphertext,
        services.clock.now(),
    )
    .await
    {
        Ok(two_factor::SetupOutcome::Started) => {
            let encoded = base32_encode(&secret);
            let uri = otpauth_uri(
                &encoded,
                services.settings.public_url.as_deref(),
                &account.normalized_email,
            );
            Json(json!({"secret": encoded, "otpauth_url": uri})).into_response()
        }
        Ok(two_factor::SetupOutcome::AlreadyEnabled) => error_response(
            StatusCode::CONFLICT,
            "TWO_FACTOR_ALREADY_ENABLED",
            "Two-factor authentication is already enabled.",
            &headers,
        ),
        Ok(two_factor::SetupOutcome::Pending) => error_response(
            StatusCode::CONFLICT,
            "TWO_FACTOR_SETUP_PENDING",
            "Another session is already setting up two-factor authentication. Finish it there or wait a few minutes.",
            &headers,
        ),
        Ok(two_factor::SetupOutcome::SessionGone) => invalid(&headers),
        Err(error) => failed(&error, &headers),
    }
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn uri_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Builds the Key URI Format provisioning URI. The label identifies the
/// issuer, the server host and the account so authenticator entries from
/// several CraftCloud servers stay distinguishable.
pub(crate) fn otpauth_uri(secret: &str, public_url: Option<&str>, email: &str) -> String {
    let host = public_url
        .map(|url| url.split_once("://").map_or(url, |(_, rest)| rest))
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .filter(|host| !host.is_empty());
    let issuer = match host {
        Some(host) => format!("CraftCloud ({host})"),
        None => "CraftCloud".to_owned(),
    };
    let issuer = uri_encode(&issuer);
    // The issuer parameter repeats the label prefix, as the Key URI Format asks.
    format!(
        "otpauth://totp/{issuer}:{}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits=6&period=30",
        uri_encode(email),
    )
}

pub(crate) async fn confirm(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    session: AuthenticatedSession,
    body: Result<Json<ConfirmBody>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Json(body) = match body {
        Ok(value) => value,
        Err(error) => return rejection_response(&error, &headers),
    };
    let reservation = match management_attempt(&services, &session, ip.map(|v| v.0), &headers) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let record = match two_factor::secret(&services.pool, session.user_id).await {
        Ok(Some(row))
            if row.enabled_at.is_none()
                && row.pending_session_id == Some(session.session_id)
                && row.pending_session_kind.as_deref() == Some(session_kind(&session))
                && row
                    .pending_expires_at
                    .is_some_and(|expires| expires > services.clock.now()) =>
        {
            row
        }
        Ok(_) => return invalid(&headers),
        Err(error) => return failed(&error, &headers),
    };
    let Some(secret) = decrypt_for_keys(&services, session.user_id, &record.secret_ciphertext)
    else {
        return unavailable(&headers, "TWO_FACTOR_UNAVAILABLE");
    };
    let Some(step) = verify_totp(
        &secret,
        &body.code,
        services.clock.now().unix_timestamp(),
        None,
    )
    .ok()
    .flatten() else {
        return management_failure(&services, reservation, &session, &headers);
    };
    let codes = match recovery_codes() {
        Ok(value) => value,
        Err(_) => return internal(&headers),
    };
    let mut hashes = Vec::with_capacity(codes.len());
    for code in &codes {
        let Ok((salt, hash)) = hash_recovery_code(code) else {
            return internal(&headers);
        };
        hashes.push(two_factor::RecoveryHash {
            salt: salt.to_vec(),
            code_hash: hash.to_vec(),
        });
    }
    match two_factor::confirm_setup_with_email(
        &services.pool,
        session.user_id,
        session.session_id,
        session_kind(&session),
        &record.secret_ciphertext,
        step,
        &hashes,
        services.clock.now(),
        &crate::audit::ctx(&headers, Some(session.user_id)),
        services.email_delivery_enabled(),
    )
    .await
    {
        Ok(two_factor::FactorChangeOutcome::Changed) => {
            reservation.record_success();
            Json(json!({"recovery_codes": codes})).into_response()
        }
        Ok(two_factor::FactorChangeOutcome::Unchanged) => {
            management_failure(&services, reservation, &session, &headers)
        }
        Err(error) => failed(&error, &headers),
    }
}

pub(crate) fn decrypt_for_keys(
    services: &Services,
    user_id: Uuid,
    ciphertext: &[u8],
) -> Option<Vec<u8>> {
    services
        .two_factor_keys
        .current
        .as_ref()
        .and_then(|key| decrypt_secret_for(key, ciphertext, user_id, b"totp").ok())
        .or_else(|| {
            services
                .two_factor_keys
                .old
                .as_ref()
                .and_then(|key| decrypt_secret_for(key, ciphertext, user_id, b"totp").ok())
        })
}

pub(crate) fn invalid(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "INVALID_CREDENTIALS",
        "The provided credentials are incorrect.",
        headers,
    )
}

pub(crate) async fn verify_account_password(
    services: &Services,
    user_id: Uuid,
    password: String,
) -> Result<Option<String>, DbError> {
    let Some(account) = accounts::get(&services.pool, user_id).await? else {
        return Ok(None);
    };
    let Some(login) = accounts::find_login(&services.pool, &account.normalized_email).await? else {
        return Ok(None);
    };
    let stored = login.password_hash;
    let verify_hash = stored.clone();
    let mut candidate = password;
    let verified = services
        .run_blocking(move || {
            let verified = verify_or_dummy(verify_hash.as_deref(), &candidate);
            candidate.zeroize();
            verified
        })
        .await
        .unwrap_or(false);
    Ok(if verified { stored } else { None })
}

pub(crate) async fn disable(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    session: AuthenticatedSession,
    body: Result<Json<DisableBody>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Json(mut body) = match body {
        Ok(value) => value,
        Err(error) => return rejection_response(&error, &headers),
    };
    let reservation = match management_attempt(&services, &session, ip.map(|v| v.0), &headers) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let password = std::mem::take(&mut body.password);
    let Some(verified_hash) =
        (match verify_account_password(&services, session.user_id, password).await {
            Ok(value) => value,
            Err(error) => return failed(&error, &headers),
        })
    else {
        return management_failure(&services, reservation, &session, &headers);
    };
    let record = match two_factor::secret(&services.pool, session.user_id).await {
        Ok(Some(row)) if row.enabled_at.is_some() => row,
        Ok(_) => return invalid(&headers),
        Err(error) => return failed(&error, &headers),
    };
    if record.failed_attempts >= two_factor::MAX_FAILED_ATTEMPTS {
        // Locked until an operator resets the factor; same answer as a wrong proof.
        return management_failure(&services, reservation, &session, &headers);
    }
    let proof = match (body.code.as_deref(), body.recovery_code.as_deref()) {
        (Some(code), None) => {
            let Some(secret) =
                decrypt_for_keys(&services, session.user_id, &record.secret_ciphertext)
            else {
                return unavailable(&headers, "TWO_FACTOR_UNAVAILABLE");
            };
            verify_totp(
                &secret,
                code,
                services.clock.now().unix_timestamp(),
                record.last_accepted_step,
            )
            .ok()
            .flatten()
            .map(two_factor::VerifiedFactor::TotpStep)
        }
        (None, Some(code)) => verify_recovery_code(&services, session.user_id, code).await,
        _ => None,
    };
    let Some(proof) = proof else {
        return proof_failure(&services, reservation, &session, &headers).await;
    };
    match two_factor::reset_for_user_with_email(
        &services.pool,
        session.user_id,
        Some(session.session_id),
        Some(session_kind(&session)),
        Some(&verified_hash),
        Some(&record.secret_ciphertext),
        Some(proof),
        services.clock.now(),
        &crate::audit::ctx(&headers, Some(session.user_id)),
        "auth.two_factor_disabled",
        services.email_delivery_enabled(),
    )
    .await
    {
        Ok(two_factor::FactorChangeOutcome::Changed) => {
            reservation.record_success();
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(two_factor::FactorChangeOutcome::Unchanged) => {
            management_failure(&services, reservation, &session, &headers)
        }
        Err(error) => failed(&error, &headers),
    }
}

pub(crate) async fn verify_recovery_code(
    services: &Services,
    user_id: Uuid,
    code: &str,
) -> Option<two_factor::VerifiedFactor> {
    let hashes = match two_factor::recovery_hashes(&services.pool, user_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "recovery code verification failed");
            return None;
        }
    };
    let candidate = code.trim().to_ascii_uppercase();
    for hash in hashes {
        let (Ok(salt), Ok(expected)) = (
            <[u8; 16]>::try_from(hash.salt.as_slice()),
            <[u8; 32]>::try_from(hash.code_hash.as_slice()),
        ) else {
            continue;
        };
        if bool::from(recovery_code_hash(&salt, &candidate).ct_eq(&expected)) {
            return Some(two_factor::VerifiedFactor::RecoverySalt(salt));
        }
    }
    None
}

pub(crate) async fn regenerate(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    session: AuthenticatedSession,
    body: Result<Json<RecoveryBody>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Json(body) = match body {
        Ok(value) => value,
        Err(error) => return rejection_response(&error, &headers),
    };
    let reservation = match management_attempt(&services, &session, ip.map(|v| v.0), &headers) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let record = match two_factor::secret(&services.pool, session.user_id).await {
        Ok(Some(row)) if row.enabled_at.is_some() => row,
        Ok(_) => return invalid(&headers),
        Err(error) => return failed(&error, &headers),
    };
    if record.failed_attempts >= two_factor::MAX_FAILED_ATTEMPTS {
        // Locked until an operator resets the factor; same answer as a wrong proof.
        return management_failure(&services, reservation, &session, &headers);
    }
    let proof = match (body.code.as_deref(), body.recovery_code.as_deref()) {
        (Some(code), None) => {
            let Some(secret) =
                decrypt_for_keys(&services, session.user_id, &record.secret_ciphertext)
            else {
                return unavailable(&headers, "TWO_FACTOR_UNAVAILABLE");
            };
            verify_totp(
                &secret,
                code,
                services.clock.now().unix_timestamp(),
                record.last_accepted_step,
            )
            .ok()
            .flatten()
            .map(two_factor::VerifiedFactor::TotpStep)
        }
        (None, Some(code)) => verify_recovery_code(&services, session.user_id, code).await,
        _ => None,
    };
    let Some(proof) = proof else {
        return proof_failure(&services, reservation, &session, &headers).await;
    };
    let codes = match recovery_codes() {
        Ok(value) => value,
        Err(_) => return internal(&headers),
    };
    let mut hashes = Vec::with_capacity(codes.len());
    for code in &codes {
        let Ok((salt, hash)) = hash_recovery_code(code) else {
            return internal(&headers);
        };
        hashes.push(two_factor::RecoveryHash {
            salt: salt.to_vec(),
            code_hash: hash.to_vec(),
        });
    }
    match two_factor::replace_recovery_codes_verified(
        &services.pool,
        session.user_id,
        session.session_id,
        session_kind(&session),
        &record.secret_ciphertext,
        proof,
        &hashes,
        services.clock.now(),
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(true) => {
            reservation.record_success();
            Json(json!({"recovery_codes": codes})).into_response()
        }
        Ok(false) => management_failure(&services, reservation, &session, &headers),
        Err(error) => failed(&error, &headers),
    }
}

async fn validate_challenge(
    services: &Services,
    headers: &HeaderMap,
    ip: Option<ClientIp>,
    body: ChallengeBody,
    kind: &str,
) -> Result<
    Option<(
        two_factor::LoginChallenge,
        String,
        two_factor::VerifiedFactor,
        [u8; 32],
        String,
        Vec<u8>,
        crate::ratelimit::Reservation,
    )>,
    Box<Response>,
> {
    crate::auth::cleanup(services).await;
    let Some(id_hash) = challenge_hash(&body.challenge_id) else {
        return Err(Box::new(invalid(headers)));
    };
    let address = ip
        .map(|value| value.0.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let challenge = match two_factor::challenge(
        &services.pool,
        &id_hash,
        kind,
        &address,
        services.clock.now(),
    )
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return Err(Box::new(invalid(headers))),
        Err(error) => return Err(Box::new(failed(&error, headers))),
    };
    let Some(account) = (match accounts::get(&services.pool, challenge.user_id).await {
        Ok(value) => value,
        Err(error) => return Err(Box::new(failed(&error, headers))),
    }) else {
        return Err(Box::new(invalid(headers)));
    };
    let reservation = match services.login_reserve(
        &account.normalized_email,
        ip.map(|client| client.0),
        headers,
    ) {
        Ok(value) => value,
        Err(response) => return Err(response),
    };
    let login = match accounts::find_login(&services.pool, &account.normalized_email).await {
        Ok(Some(value)) if !value.disabled => value,
        Ok(_) => return Err(Box::new(invalid(headers))),
        Err(error) => return Err(Box::new(failed(&error, headers))),
    };
    let Some(password_hash) = login.password_hash else {
        return Err(Box::new(invalid(headers)));
    };
    let digest: [u8; 32] = Sha256::digest(password_hash.as_bytes()).into();
    if !bool::from(digest.ct_eq(challenge.password_hash_digest.as_slice())) {
        reservation.record_failure(services.now_secs());
        crate::auth::record_two_factor_failure(services, headers, challenge.user_id);
        return Err(Box::new(invalid(headers)));
    }
    let factor = match two_factor::secret(&services.pool, challenge.user_id).await {
        Ok(Some(value)) if value.enabled_at.is_some() => value,
        Ok(_) => return Err(Box::new(invalid(headers))),
        Err(error) => return Err(Box::new(failed(&error, headers))),
    };
    if factor.failed_attempts >= two_factor::MAX_FAILED_ATTEMPTS {
        // Locked until an operator resets the factor; same shape as a wrong code.
        reservation.record_failure(services.now_secs());
        // Keep the failure visible in the audit trail; the counter stays capped.
        crate::auth::record_two_factor_failure(services, headers, challenge.user_id);
        return Err(Box::new(invalid(headers)));
    }
    let Some(secret) = decrypt_for_keys(services, challenge.user_id, &factor.secret_ciphertext)
    else {
        return Err(Box::new(unavailable(headers, "TWO_FACTOR_UNAVAILABLE")));
    };
    let verified_factor = match (body.code.as_deref(), body.recovery_code.as_deref()) {
        (Some(code), None) => verify_totp(
            &secret,
            code,
            services.clock.now().unix_timestamp(),
            factor.last_accepted_step,
        )
        .ok()
        .flatten()
        .map(two_factor::VerifiedFactor::TotpStep),
        (None, Some(code)) => {
            let hashes = match two_factor::recovery_hashes(&services.pool, challenge.user_id).await
            {
                Ok(value) => value,
                Err(error) => return Err(Box::new(failed(&error, headers))),
            };
            let candidate = code.trim().to_ascii_uppercase();
            let mut accepted = None;
            for hash in hashes {
                let (Ok(salt), Ok(expected)) = (
                    <[u8; 16]>::try_from(hash.salt.as_slice()),
                    <[u8; 32]>::try_from(hash.code_hash.as_slice()),
                ) else {
                    continue;
                };
                if bool::from(recovery_code_hash(&salt, &candidate).ct_eq(&expected)) {
                    accepted = Some(two_factor::VerifiedFactor::RecoverySalt(salt));
                    break;
                }
            }
            accepted
        }
        _ => None,
    };
    let Some(verified_factor) = verified_factor else {
        reservation.record_failure(services.now_secs());
        crate::auth::record_two_factor_failure(services, headers, challenge.user_id);
        if let Err(error) =
            two_factor::failed_attempt(&services.pool, &id_hash, services.clock.now()).await
        {
            return Err(Box::new(failed(&error, headers)));
        }
        if let Err(error) = two_factor::record_failed_proof(
            &services.pool,
            challenge.user_id,
            &crate::audit::ctx(headers, Some(challenge.user_id)),
        )
        .await
        {
            return Err(Box::new(failed(&error, headers)));
        }
        return Err(Box::new(invalid(headers)));
    };
    Ok(Some((
        challenge,
        password_hash,
        verified_factor,
        id_hash,
        address,
        factor.secret_ciphertext,
        reservation,
    )))
}

pub(crate) async fn browser_login(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    body: Result<Json<ChallengeBody>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if !crate::session::origin_ok(&headers, &services.settings) {
        return error_response(
            StatusCode::FORBIDDEN,
            "CSRF_FAILED",
            "The request failed the cross-site request forgery check.",
            &headers,
        );
    }
    if let Err(response) = services.rate_limit_ip(
        crate::services::IpClass::Login,
        ip.map(|Extension(value)| value.0),
        &headers,
    ) {
        return *response;
    }
    let Json(body) = match body {
        Ok(value) => value,
        Err(error) => return rejection_response(&error, &headers),
    };
    let (
        challenge,
        password_hash,
        verified_factor,
        id_hash,
        address,
        secret_ciphertext,
        reservation,
    ) = match validate_challenge(
        &services,
        &headers,
        ip.map(|Extension(value)| value),
        body,
        "browser",
    )
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return invalid(&headers),
        Err(response) => return *response,
    };
    let Ok(token) = OpaqueToken::generate() else {
        return internal(&headers);
    };
    let now = services.clock.now();
    let absolute = now + services.settings.absolute_timeout;
    match two_factor::complete_browser_login_with_email(
        &services.pool,
        &id_hash,
        challenge.user_id,
        &address,
        &password_hash,
        &secret_ciphertext,
        verified_factor,
        &sessions::NewSession {
            user_id: challenge.user_id,
            token_hash: token.hash(),
            now,
            absolute_expires_at: absolute,
        },
        &crate::audit::ctx(&headers, Some(challenge.user_id)),
        services.email_delivery_enabled(),
    )
    .await
    {
        Ok(two_factor::LoginOutcome::Created(_)) => reservation.record_success(),
        Ok(two_factor::LoginOutcome::AppNotAssigned | two_factor::LoginOutcome::Rejected) => {
            reservation.record_failure(services.now_secs());
            return invalid(&headers);
        }
        Err(error) => return failed(&error, &headers),
    }
    if let Some(old) = crate::session::session_token(&headers, &services.settings) {
        let _ = sessions::revoke_by_token_hash(&services.pool, &old.hash(), now).await;
    }
    crate::auth::cleanup(&services).await;
    let expires = (now + services.settings.idle_timeout).min(absolute);
    let max_age = (absolute - now).whole_seconds();
    let cookie = crate::session::session_cookie(&services.settings, &token, max_age);
    crate::auth::with_cookie(Json(json!({"user_id":challenge.user_id.to_string(),"csrf_token":token.csrf_token(),"expires_at":crate::auth::rfc3339(expires)})).into_response(), cookie)
}

pub(crate) async fn device_login(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    trusted_proxy: Option<Extension<TrustedProxy>>,
    body: Result<Json<ChallengeBody>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if crate::session::ambiguous_credentials(&headers, &services.settings) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "AMBIGUOUS_CREDENTIALS",
            "Send either a session cookie or a bearer token, not both.",
            &headers,
        );
    }
    if !crate::device::tls_ready(&services, &headers, trusted_proxy) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "DEVICE_TLS_REQUIRED",
            "Desktop sign-in requires HTTPS.",
            &headers,
        );
    }
    if let Err(response) = services.rate_limit_ip(
        crate::services::IpClass::Login,
        ip.map(|Extension(value)| value.0),
        &headers,
    ) {
        return *response;
    }
    let Json(body) = match body {
        Ok(value) => value,
        Err(error) => return rejection_response(&error, &headers),
    };
    let (
        challenge,
        password_hash,
        verified_factor,
        id_hash,
        address,
        secret_ciphertext,
        reservation,
    ) = match validate_challenge(
        &services,
        &headers,
        ip.map(|Extension(value)| value),
        body,
        "device",
    )
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return invalid(&headers),
        Err(response) => return *response,
    };
    let (Some(client_kind), Some(client_id), Some(device_name)) = (
        challenge.device_client_kind,
        challenge.device_client_id,
        challenge.device_name,
    ) else {
        reservation.record_failure(services.now_secs());
        crate::auth::record_two_factor_failure(&services, &headers, challenge.user_id);
        return invalid(&headers);
    };
    // The contract answers with the same `DeviceLoginResponse` as the password-only
    // route, including the signed-in user. Read it before the session is created so a
    // lookup failure cannot leave a session the client never learns about.
    let public = match accounts::get(&services.pool, challenge.user_id).await {
        Ok(Some(public)) => public,
        Ok(None) => return invalid(&headers),
        Err(error) => return failed(&error, &headers),
    };
    let user = json!({"user_id":public.id.to_string(),"email":public.normalized_email,"display_name":public.display_name,"instance_role":public.instance_role});
    let Ok(access) = DeviceToken::generate(DeviceTokenKind::Access) else {
        return internal(&headers);
    };
    let Ok(refresh) = DeviceToken::generate(DeviceTokenKind::Refresh) else {
        return internal(&headers);
    };
    let now = services.clock.now();
    let (access_expiry, idle_expiry, absolute) = crate::device::device_expiries(
        now,
        services.settings.device_access_ttl,
        services.settings.device_idle_timeout,
        services.settings.device_absolute_timeout,
    );
    let new = NewDeviceSession {
        user_id: challenge.user_id,
        name: device_name,
        client_kind,
        client_id,
        known_app_ids: services
            .settings
            .app_catalog
            .iter()
            .map(|app| app.id.clone())
            .collect(),
        refresh_hash: refresh.hash(),
        access_hash: access.hash(),
        now,
        access_expires_at: access_expiry,
        idle_expires_at: idle_expiry,
        absolute_expires_at: absolute,
    };
    let id = match two_factor::complete_device_login_with_email(
        &services.pool,
        &id_hash,
        challenge.user_id,
        &address,
        &password_hash,
        &secret_ciphertext,
        verified_factor,
        &new,
        &crate::audit::ctx(&headers, Some(challenge.user_id)),
        services.email_delivery_enabled(),
    )
    .await
    {
        Ok(two_factor::LoginOutcome::Created(id)) => {
            reservation.record_success();
            id
        }
        Ok(two_factor::LoginOutcome::AppNotAssigned) => {
            return error_response(
                StatusCode::FORBIDDEN,
                "APP_NOT_ASSIGNED",
                "Your administrator has not assigned this app for cloud access.",
                &headers,
            );
        }
        Ok(two_factor::LoginOutcome::Rejected) => {
            reservation.record_failure(services.now_secs());
            return invalid(&headers);
        }
        Err(error) => return failed(&error, &headers),
    };
    crate::auth::cleanup(&services).await;
    crate::device::token_reply(
        id,
        &access,
        &refresh,
        access_expiry,
        idle_expiry,
        Some(user),
    )
}

#[cfg(test)]
mod tests {
    use super::{otpauth_uri, uri_encode};

    #[test]
    fn labels_are_percent_encoded_per_the_key_uri_format() {
        assert_eq!(uri_encode("a b(c)@d.e"), "a%20b%28c%29%40d.e");
        assert_eq!(
            otpauth_uri(
                "ABC",
                Some("https://cloud.example:8443/app"),
                "ann+x@example.test"
            ),
            "otpauth://totp/CraftCloud%20%28cloud.example%3A8443%29:ann%2Bx%40example.test?secret=ABC&issuer=CraftCloud%20%28cloud.example%3A8443%29&algorithm=SHA1&digits=6&period=30"
        );
        assert_eq!(
            otpauth_uri("ABC", None, "a@b.test"),
            "otpauth://totp/CraftCloud:a%40b.test?secret=ABC&issuer=CraftCloud&algorithm=SHA1&digits=6&period=30"
        );
        // Hostile characters in the account name cannot add URI parameters.
        let uri = otpauth_uri("ABC", Some("https://h.test"), "x&secret=EVIL#y");
        assert!(
            !uri.contains("EVIL#") && uri.matches("secret=").count() == 1,
            "{uri}"
        );
    }
}
