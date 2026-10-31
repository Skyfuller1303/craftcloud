//! Email verification, address change, and operator delivery status.

use axum::{
    Extension, Json,
    extract::{Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{OpaqueToken, verify_totp};
use craftcloud_core::NormalizedEmail;
use craftcloud_db::{DbError, accounts, email_outbox, email_verification, two_factor};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::orgs::operator_gate;
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::services::{IpClass, Services};
use crate::session::{AuthRejection, AuthenticatedSession, CredentialKind, origin_ok};
use crate::{ApiState, ClientIp, error_response};

const VERIFY_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CHANGE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const GENERIC_VERIFY: &str = "If the account can be verified, a confirmation email will be sent.";

#[derive(Deserialize)]
pub(crate) struct VerificationCursor {
    after: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenRequest {
    token: String,
}

impl Drop for TokenRequest {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmailChangeRequest {
    email: String,
    password: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

impl Drop for EmailChangeRequest {
    fn drop(&mut self) {
        self.password.zeroize();
        if let Some(code) = &mut self.code {
            code.zeroize();
        }
        if let Some(code) = &mut self.recovery_code {
            code.zeroize();
        }
    }
}

fn capability_hash(raw: &str) -> Option<[u8; 32]> {
    OpaqueToken::parse(raw).map(|token| token.hash())
}

fn queue_limit(outcome: email_outbox::EnqueueOutcome, headers: &HeaderMap) -> Option<Response> {
    (outcome != email_outbox::EnqueueOutcome::Queued).then(|| {
        error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "Too many email messages were requested. Try again later.",
            headers,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn draft(
    services: &Services,
    recipient: String,
    template: &'static str,
    name: &str,
    actor_id: Option<Uuid>,
    now: OffsetDateTime,
    expires_at: OffsetDateTime,
    token: Option<&OpaqueToken>,
    is_resend: bool,
) -> Result<email_outbox::EmailDraft, ()> {
    let id = Uuid::new_v4();
    let encrypted_token = token
        .map(|token| services.encrypt_email_token(id, token.expose().as_bytes()))
        .transpose()
        .map_err(|_| ())?;
    Ok(email_outbox::EmailDraft {
        id,
        recipient,
        template: template.to_owned(),
        language: "en".to_owned(),
        params: json!({"recipient_name": name}),
        encrypted_token,
        expires_at,
        actor_id,
        is_resend,
        now,
    })
}

pub(crate) async fn request_verification(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if !services.email_delivery_enabled() {
        return unavailable(&headers, "EMAIL_DELIVERY_UNAVAILABLE");
    }
    let account = match accounts::get(&services.pool, session.user_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return crate::auth::invalid_credentials(&headers),
        Err(error) => return crate::orgs::db_failure(&error, &headers, "account lookup failed"),
    };
    let expected_email = account.normalized_email.clone();
    if account.email_verified_at.is_none() {
        let Ok(token) = OpaqueToken::generate() else {
            return internal(&headers);
        };
        let now = services.clock.now();
        let expires_at = now + VERIFY_TTL;
        let queued = async {
            let mut tx = services.pool.begin().await?;
            let inserted = email_verification::issue_verification_in(
                &mut tx,
                session.user_id,
                &expected_email,
                &token.hash(),
                now,
                expires_at,
            )
            .await?;
            if inserted {
                let draft = draft(
                    &services,
                    account.normalized_email.clone(),
                    "email_verification",
                    &account.display_name,
                    Some(session.user_id),
                    now,
                    expires_at,
                    Some(&token),
                    true,
                )
                .map_err(|_| DbError::Internal)?;
                let outcome = email_outbox::enqueue_in(&mut tx, &draft).await?;
                if outcome != email_outbox::EnqueueOutcome::Queued {
                    tx.rollback().await?;
                    return Ok::<_, DbError>(false);
                }
            }
            tx.commit().await?;
            Ok::<_, DbError>(inserted)
        }
        .await;
        if let Err(error) = queued {
            return crate::orgs::db_failure(&error, &headers, "verification request failed");
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({"message": GENERIC_VERIFY})),
    )
        .into_response()
}

pub(crate) async fn request_all_verification(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Query(cursor): Query<VerificationCursor>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if !services.email_delivery_enabled() {
        return unavailable(&headers, "EMAIL_DELIVERY_UNAVAILABLE");
    }
    if let Err(response) = operator_gate(
        &services,
        &session,
        craftcloud_core::policy::Action::PasswordResetIssue,
        "email_verification.bulk_issue",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    let now = services.clock.now();
    let after = match cursor.after.as_deref().map(Uuid::parse_str).transpose() {
        Ok(after) => after,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_CURSOR",
                "The verification cursor is invalid.",
                &headers,
            );
        }
    };
    let mut users = match email_verification::list_unverified(&services.pool, now, after, 501).await
    {
        Ok(users) => users,
        Err(error) => {
            return crate::orgs::db_failure(&error, &headers, "unverified account lookup failed");
        }
    };
    let has_more = users.len() > 500;
    users.truncate(500);
    let expires_at = now + VERIFY_TTL;
    let mut queued_count = 0_u64;
    let mut next_cursor = cursor.after;
    let mut limited = false;
    for user in users {
        let Ok(token) = OpaqueToken::generate() else {
            return internal(&headers);
        };
        let draft = match draft(
            &services,
            user.normalized_email.clone(),
            "email_verification",
            &user.display_name,
            Some(session.user_id),
            now,
            expires_at,
            Some(&token),
            true,
        ) {
            Ok(draft) => draft,
            Err(()) => return internal(&headers),
        };
        let result = async {
            let mut tx = services.pool.begin().await?;
            let inserted = email_verification::issue_verification_in(
                &mut tx,
                user.user_id,
                &user.normalized_email,
                &token.hash(),
                now,
                expires_at,
            )
            .await?;
            if !inserted {
                tx.rollback().await?;
                return Ok::<_, DbError>(None);
            }
            let outcome = email_outbox::enqueue_in(&mut tx, &draft).await?;
            if outcome != email_outbox::EnqueueOutcome::Queued {
                tx.rollback().await?;
                return Ok(Some(outcome));
            }
            tx.commit().await?;
            Ok(Some(email_outbox::EnqueueOutcome::Queued))
        }
        .await;
        match result {
            Ok(Some(email_outbox::EnqueueOutcome::Queued)) => {
                queued_count = queued_count.saturating_add(1);
                next_cursor = Some(user.user_id.to_string());
            }
            Ok(Some(outcome)) => {
                if outcome == email_outbox::EnqueueOutcome::ActorLimit {
                    limited = true;
                    break;
                }
                next_cursor = Some(user.user_id.to_string());
            }
            Ok(None) => next_cursor = Some(user.user_id.to_string()),
            Err(error) => {
                return crate::orgs::db_failure(
                    &error,
                    &headers,
                    "bulk verification enqueue failed",
                );
            }
        }
    }
    if !limited && !has_more {
        next_cursor = None;
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "queued": queued_count,
            "next_cursor": next_cursor,
            "limited": limited,
            "retry_after_seconds": if limited { Some(3600) } else { None },
        })),
    )
        .into_response()
}

pub(crate) async fn confirm_verification(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    session: Result<AuthenticatedSession, AuthRejection>,
    body: Result<Json<TokenRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) =
        services.rate_limit_ip(IpClass::Accept, ip.map(|value| value.0.0), &headers)
    {
        return *response;
    }
    let session = match session {
        Ok(session) => Some(session),
        Err(rejection) if rejection.is_unauthenticated() => None,
        Err(rejection) => return rejection.into_response(),
    };
    let bearer_actor = session
        .as_ref()
        .is_some_and(|value| value.credential_kind == CredentialKind::Device);
    if !bearer_actor && !origin_ok(&headers, &services.settings) {
        return error_response(
            StatusCode::FORBIDDEN,
            "CSRF_FAILED",
            "The request failed the cross-site request forgery check.",
            &headers,
        );
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return rejection_response(&error, &headers),
    };
    let Some(hash) = capability_hash(&body.token) else {
        return invalid_email_token(&headers);
    };
    match email_verification::confirm_verification(&services.pool, &hash, services.clock.now())
        .await
    {
        Ok(Some(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(None) => invalid_email_token(&headers),
        Err(error) => crate::orgs::db_failure(&error, &headers, "email verification failed"),
    }
}

pub(crate) async fn change_email(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    session: AuthenticatedSession,
    body: Result<Json<EmailChangeRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if !services.email_delivery_enabled() {
        return unavailable(&headers, "EMAIL_DELIVERY_UNAVAILABLE");
    }
    let Json(mut body) = match body {
        Ok(body) => body,
        Err(error) => return rejection_response(&error, &headers),
    };
    let email = match NormalizedEmail::parse(&body.email) {
        Ok(email) => email,
        Err(_) => return validation_failed(&["email"], &headers),
    };
    let account = match accounts::get(&services.pool, session.user_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return crate::auth::invalid_credentials(&headers),
        Err(error) => return crate::orgs::db_failure(&error, &headers, "account lookup failed"),
    };
    if account.normalized_email == email.as_str() {
        return validation_failed(&["email"], &headers);
    }
    let reservation = match crate::two_factor::management_attempt(
        &services,
        &session,
        ip.map(|value| value.0),
        &headers,
    ) {
        Ok(reservation) => reservation,
        Err(response) => return *response,
    };
    let password = std::mem::take(&mut body.password);
    let verified_hash = match crate::two_factor::verify_account_password(
        &services,
        session.user_id,
        password,
    )
    .await
    {
        Ok(Some(hash)) => hash,
        Ok(None) => {
            reservation.record_failure(services.now_secs());
            return crate::two_factor::invalid(&headers);
        }
        Err(error) => return crate::orgs::db_failure(&error, &headers, "password check failed"),
    };
    let factor = match two_factor::secret(&services.pool, session.user_id).await {
        Ok(value) if value.as_ref().is_some_and(|row| row.enabled_at.is_some()) => value,
        Ok(_) => None,
        Err(error) => return crate::orgs::db_failure(&error, &headers, "factor lookup failed"),
    };
    let proof = match factor.as_ref() {
        None if body.code.is_none() && body.recovery_code.is_none() => None,
        Some(record)
            if record.enabled_at.is_some()
                && record.failed_attempts < two_factor::MAX_FAILED_ATTEMPTS =>
        {
            match (body.code.as_deref(), body.recovery_code.as_deref()) {
                (Some(code), None) => {
                    let Some(mut secret) = crate::two_factor::decrypt_for_keys(
                        &services,
                        session.user_id,
                        &record.secret_ciphertext,
                    ) else {
                        return unavailable(&headers, "TWO_FACTOR_UNAVAILABLE");
                    };
                    let proof = verify_totp(
                        &secret,
                        code,
                        services.clock.now().unix_timestamp(),
                        record.last_accepted_step,
                    )
                    .ok()
                    .flatten()
                    .map(two_factor::VerifiedFactor::TotpStep);
                    secret.zeroize();
                    proof.map(|proof| (proof, record.secret_ciphertext.clone()))
                }
                (None, Some(code)) => {
                    crate::two_factor::verify_recovery_code(&services, session.user_id, code)
                        .await
                        .map(|proof| (proof, record.secret_ciphertext.clone()))
                }
                _ => None,
            }
        }
        _ => None,
    };
    if factor
        .as_ref()
        .is_some_and(|record| record.enabled_at.is_some())
        && proof.is_none()
        || (factor.is_none() && (body.code.is_some() || body.recovery_code.is_some()))
    {
        return crate::two_factor::proof_failure(&services, reservation, &session, &headers).await;
    }
    let Ok(token) = OpaqueToken::generate() else {
        return internal(&headers);
    };
    let now = services.clock.now();
    let expires_at = now + CHANGE_TTL;
    let target = email.as_str().to_owned();
    let mut old_notice = match draft(
        &services,
        account.normalized_email.clone(),
        "security_notice",
        &account.display_name,
        Some(session.user_id),
        now,
        expires_at,
        None,
        false,
    ) {
        Ok(draft) => draft,
        Err(()) => return internal(&headers),
    };
    old_notice.params = json!({
        "recipient_name": account.display_name.clone(),
        "security_event": "email address change requested"
    });
    let outcome = async {
        let mut tx = services.pool.begin().await?;
        let proof_ref = proof
            .as_ref()
            .map(|(proof, ciphertext)| (ciphertext.as_slice(), *proof));
        let session_proof = email_verification::EmailChangeSession {
            id: session.session_id,
            device: session.credential_kind == CredentialKind::Device,
            expires_at: session.expires_at,
        };
        let staged = email_verification::stage_email_change_verified_in(
            &mut tx,
            session.user_id,
            &verified_hash,
            &target,
            &token.hash(),
            now,
            expires_at,
            proof_ref,
            session_proof,
        )
        .await?;
        // An unavailable address does the same observable work as a free one:
        // the same rate-window accounting for the target and the same notice
        // to the old address. Only the delivery of the link is skipped.
        let deliverable = match staged {
            email_verification::StageEmailChangeOutcome::ProofInvalid => {
                tx.rollback().await?;
                return Ok::<_, DbError>(None);
            }
            email_verification::StageEmailChangeOutcome::AddressUnavailable => false,
            email_verification::StageEmailChangeOutcome::Staged => true,
        };
        let mut mail = draft(
            &services,
            target.clone(),
            "email_change",
            &account.display_name,
            Some(session.user_id),
            now,
            expires_at,
            Some(&token),
            true,
        )
        .map_err(|_| DbError::Internal)?;
        mail.params = json!({
            "recipient_name": account.display_name,
            "new_email": target
        });
        let queued = if deliverable {
            email_outbox::enqueue_in(&mut tx, &mail).await?
        } else {
            email_outbox::enqueue_discarded_in(&mut tx, &mail).await?
        };
        if queued != email_outbox::EnqueueOutcome::Queued {
            tx.rollback().await?;
            return Ok(Some(queued));
        }
        // The notice to the old address never blocks the request.
        email_outbox::enqueue_notice_in(&mut tx, &old_notice).await?;
        tx.commit().await?;
        Ok(Some(email_outbox::EnqueueOutcome::Queued))
    }
    .await;
    match outcome {
        Ok(Some(_)) => {
            reservation.record_success();
            (
                StatusCode::ACCEPTED,
                Json(json!({"message": GENERIC_VERIFY})),
            )
                .into_response()
        }
        Ok(None) => {
            crate::two_factor::proof_failure(&services, reservation, &session, &headers).await
        }
        Err(error) => crate::orgs::db_failure(&error, &headers, "email change request failed"),
    }
}

pub(crate) async fn confirm_change(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    session: Result<AuthenticatedSession, AuthRejection>,
    body: Result<Json<TokenRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) =
        services.rate_limit_ip(IpClass::Accept, ip.map(|value| value.0.0), &headers)
    {
        return *response;
    }
    let session = match session {
        Ok(session) => Some(session),
        Err(rejection) if rejection.is_unauthenticated() => None,
        Err(rejection) => return rejection.into_response(),
    };
    let bearer_actor = session
        .as_ref()
        .is_some_and(|value| value.credential_kind == CredentialKind::Device);
    if !bearer_actor && !origin_ok(&headers, &services.settings) {
        return error_response(
            StatusCode::FORBIDDEN,
            "CSRF_FAILED",
            "The request failed the cross-site request forgery check.",
            &headers,
        );
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return rejection_response(&error, &headers),
    };
    let Some(hash) = capability_hash(&body.token) else {
        return invalid_email_token(&headers);
    };
    match email_verification::confirm_email_change(&services.pool, &hash, services.clock.now())
        .await
    {
        Ok(email_verification::ConfirmEmailChangeOutcome::Confirmed) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(email_verification::ConfirmEmailChangeOutcome::Invalid) => invalid_email_token(&headers),
        Err(error) => crate::orgs::db_failure(&error, &headers, "email change confirmation failed"),
    }
}

pub(crate) async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = operator_gate(
        &services,
        &session,
        craftcloud_core::policy::Action::PasswordResetIssue,
        "email_delivery.status",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    let status = match email_outbox::status(&services.pool).await {
        Ok(status) => status,
        Err(error) => {
            return crate::orgs::db_failure(&error, &headers, "email status lookup failed");
        }
    };
    Json(json!({
        "configured": services.email_delivery_enabled(),
        "last_success_at": status.last_success_at.map(crate::orgs::rfc3339),
        "failed_count": status.failed_count,
        "suppressed_count": status.suppressed_count,
    }))
    .into_response()
}

pub(crate) async fn test_email(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if !services.email_delivery_enabled() {
        return unavailable(&headers, "EMAIL_DELIVERY_UNAVAILABLE");
    }
    if let Err(response) = operator_gate(
        &services,
        &session,
        craftcloud_core::policy::Action::PasswordResetIssue,
        "email_delivery.test",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    let account = match accounts::get(&services.pool, session.user_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return crate::auth::invalid_credentials(&headers),
        Err(error) => return crate::orgs::db_failure(&error, &headers, "account lookup failed"),
    };
    let now = services.clock.now();
    let Ok(draft) = draft(
        &services,
        account.normalized_email,
        "test",
        &account.display_name,
        Some(session.user_id),
        now,
        now + Duration::from_secs(3600),
        None,
        true,
    ) else {
        return internal(&headers);
    };
    let mut tx = match services.pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            return crate::orgs::db_failure(&error.into(), &headers, "test mail enqueue failed");
        }
    };
    match email_outbox::enqueue_in(&mut tx, &draft).await {
        Ok(email_outbox::EnqueueOutcome::Queued) => match tx.commit().await {
            Ok(()) => (StatusCode::ACCEPTED, Json(json!({"message": "queued"}))).into_response(),
            Err(error) => {
                crate::orgs::db_failure(&error.into(), &headers, "test mail enqueue failed")
            }
        },
        Ok(outcome) => {
            let _ = tx.rollback().await;
            queue_limit(outcome, &headers).unwrap_or_else(|| internal(&headers))
        }
        Err(error) => crate::orgs::db_failure(&error, &headers, "test mail enqueue failed"),
    }
}

fn invalid_email_token(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "INVALID_EMAIL_TOKEN",
        "The email link is invalid, expired or already used.",
        headers,
    )
}
