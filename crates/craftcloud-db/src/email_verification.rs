//! Single-use email verification and pending address changes.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, NewEvent, record_in};
use crate::two_factor::VerifiedFactor;
use sha2::{Digest, Sha256};

/// An active account awaiting address verification.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UnverifiedAccount {
    /// Account identifier.
    pub user_id: Uuid,
    /// Current normalized e-mail address.
    pub normalized_email: String,
    /// Display name for the message template.
    pub display_name: String,
}

/// Result of a password and second-factor protected address-change request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageEmailChangeOutcome {
    /// Proofs were valid and the target was reserved.
    Staged,
    /// Proofs were valid, but the target is already in use.
    AddressUnavailable,
    /// Password or second-factor proof is stale or invalid.
    ProofInvalid,
}

/// Outcome of consuming a pending e-mail change capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmEmailChangeOutcome {
    /// Invalid, expired, consumed or stale capability.
    Invalid,
    /// Changed successfully and notified the old address.
    Confirmed,
    /// Durable mail limits prevented the notice; change was rolled back.
    RateLimited,
}

/// Authenticated session evidence that must remain valid through password work.
#[derive(Debug, Clone, Copy)]
pub struct EmailChangeSession {
    /// Session record identifier.
    pub id: Uuid,
    /// Whether this is a device access session rather than a browser session.
    pub device: bool,
    /// Expiry computed by the authentication extractor.
    pub expires_at: OffsetDateTime,
}

/// Lists active unverified accounts in stable order, with a bounded result.
pub async fn list_unverified(
    pool: &PgPool,
    now: OffsetDateTime,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<UnverifiedAccount>, DbError> {
    let limit = limit.clamp(1, 501);
    Ok(sqlx::query_as(
        "SELECT id AS user_id,normalized_email,display_name FROM users \
         WHERE disabled_at IS NULL AND email_verified_at IS NULL \
           AND NOT EXISTS(SELECT 1 FROM email_verification_tokens t WHERE t.user_id=users.id AND t.expires_at>$1) \
           AND ($2::uuid IS NULL OR id>$2) ORDER BY id LIMIT $3",
    )
    .bind(now)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Replaces outstanding verification capabilities for an account.
pub async fn issue_verification_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    expected_email: &str,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
    expires_at: OffsetDateTime,
) -> Result<bool, DbError> {
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT disabled_at IS NULL AND email_verified_at IS NULL AND normalized_email=$2 FROM users WHERE id=$1 FOR UPDATE",
    )
    .bind(user_id)
    .bind(expected_email)
    .fetch_optional(&mut **tx)
    .await?;
    if active != Some(true) {
        return Ok(false);
    }
    sqlx::query("DELETE FROM email_verification_tokens WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO email_verification_tokens(token_hash,user_id,normalized_email,created_at,expires_at) \
         SELECT $1,id,normalized_email,$3,$4 FROM users WHERE id=$2",
    )
    .bind(token_hash.as_slice())
    .bind(user_id)
    .bind(now)
    .bind(expires_at)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

/// Consumes a valid verification token and marks its account verified.
pub async fn confirm_verification(
    pool: &PgPool,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
) -> Result<Option<Uuid>, DbError> {
    let mut tx = pool.begin().await?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM email_verification_tokens WHERE token_hash=$1")
            .bind(token_hash.as_slice())
            .fetch_optional(&mut *tx)
            .await?;
    let Some(user_id) = owner else {
        return Ok(None);
    };
    let account: Option<(bool, String)> = sqlx::query_as(
        "SELECT disabled_at IS NULL AND email_verified_at IS NULL,normalized_email FROM users WHERE id=$1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((true, current_email)) = account else {
        return Ok(None);
    };
    let consumed: Option<Uuid> = sqlx::query_scalar(
        "DELETE FROM email_verification_tokens t USING users u \
         WHERE t.token_hash=$1 AND t.user_id=$2 AND t.expires_at>$3 AND t.normalized_email=$4 \
           AND u.id=t.user_id AND u.disabled_at IS NULL AND u.email_verified_at IS NULL \
         RETURNING t.user_id",
    )
    .bind(token_hash.as_slice())
    .bind(user_id)
    .bind(now)
    .bind(&current_email)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user_id) = consumed else {
        return Ok(None);
    };
    sqlx::query("UPDATE users SET email_verified_at=$2 WHERE id=$1 AND email_verified_at IS NULL AND disabled_at IS NULL")
        .bind(user_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    record_in(
        &mut tx,
        &AuditCtx::default(),
        &NewEvent::new("account.email_verified").target("user", user_id),
    )
    .await?;
    tx.commit().await?;
    Ok(Some(user_id))
}

/// Stages an address change after checking password and consuming current 2FA
/// proof under the same user lock as the staged request.
#[allow(clippy::too_many_arguments)]
pub async fn stage_email_change_verified_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    verified_password_hash: &str,
    normalized_email: &str,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
    expires_at: OffsetDateTime,
    factor_proof: Option<(&[u8], VerifiedFactor)>,
    session: EmailChangeSession,
) -> Result<StageEmailChangeOutcome, DbError> {
    if session.expires_at <= now {
        return Ok(StageEmailChangeOutcome::ProofInvalid);
    }
    let account: Option<(bool, String, i64)> = sqlx::query_as(
        "SELECT disabled_at IS NULL,normalized_email,security_version FROM users WHERE id=$1 AND password_hash=$2 FOR UPDATE",
    )
    .bind(user_id)
    .bind(verified_password_hash)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((true, original_email, security_version)) = account else {
        return Ok(StageEmailChangeOutcome::ProofInvalid);
    };
    let active_session = if session.device {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM device_sessions WHERE id=$1 AND user_id=$2 \
             AND revoked_at IS NULL AND access_expires_at>$3 AND idle_expires_at>$3 AND absolute_expires_at>$3 FOR UPDATE",
        )
        .bind(session.id)
        .bind(user_id)
        .bind(now)
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
    } else {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR UPDATE",
        )
        .bind(session.id)
        .bind(user_id)
        .bind(now)
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
    };
    if !active_session {
        return Ok(StageEmailChangeOutcome::ProofInvalid);
    }
    let current_factor: Option<(Vec<u8>, i32)> = sqlx::query_as(
        "SELECT secret_ciphertext,failed_attempts FROM user_two_factor \
         WHERE user_id=$1 AND enabled_at IS NOT NULL FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    match (current_factor, factor_proof) {
        (None, None) => {}
        (Some((ciphertext, attempts)), Some((expected_ciphertext, proof)))
            if attempts < crate::two_factor::MAX_FAILED_ATTEMPTS
                && ciphertext.as_slice() == expected_ciphertext =>
        {
            let accepted = match proof {
                VerifiedFactor::TotpStep(step) => {
                    sqlx::query(
                        "UPDATE user_two_factor SET last_accepted_step=$2,failed_attempts=0 \
                                 WHERE user_id=$1 AND enabled_at IS NOT NULL \
                                   AND (last_accepted_step IS NULL OR last_accepted_step<$2)",
                    )
                    .bind(user_id)
                    .bind(step)
                    .execute(&mut **tx)
                    .await?
                    .rows_affected()
                        == 1
                }
                VerifiedFactor::RecoverySalt(salt) => {
                    sqlx::query(
                        "UPDATE user_two_factor_recovery_codes SET used_at=$3 \
                                 WHERE user_id=$1 AND salt=$2 AND used_at IS NULL",
                    )
                    .bind(user_id)
                    .bind(salt.as_slice())
                    .bind(now)
                    .execute(&mut **tx)
                    .await?
                    .rows_affected()
                        == 1
                }
            };
            if !accepted {
                return Ok(StageEmailChangeOutcome::ProofInvalid);
            }
        }
        _ => return Ok(StageEmailChangeOutcome::ProofInvalid),
    }
    let factor_ciphertext: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT secret_ciphertext FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL",
    )
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(
        if stage_address_change(
            tx,
            user_id,
            &original_email,
            security_version,
            factor_ciphertext.as_deref(),
            normalized_email,
            token_hash,
            now,
            expires_at,
        )
        .await?
        {
            StageEmailChangeOutcome::Staged
        } else {
            StageEmailChangeOutcome::AddressUnavailable
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn stage_address_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    original_email: &str,
    security_version: i64,
    factor_ciphertext: Option<&[u8]>,
    normalized_email: &str,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
    expires_at: OffsetDateTime,
) -> Result<bool, DbError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{}:{normalized_email}", normalized_email.len()))
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM pending_email_changes WHERE normalized_email=$1 AND user_id<>$2 AND expires_at<=$3")
        .bind(normalized_email)
        .bind(user_id)
        .bind(now)
        .execute(&mut **tx)
        .await?;
    let occupied: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE normalized_email=$1 AND id<>$2) \
         OR EXISTS(SELECT 1 FROM pending_email_changes WHERE normalized_email=$1 AND user_id<>$2)",
    )
    .bind(normalized_email)
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    if occupied {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO pending_email_changes(user_id,normalized_email,token_hash,requested_at,expires_at,original_email,security_version,factor_fingerprint) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(user_id) DO UPDATE SET \
         normalized_email=EXCLUDED.normalized_email,token_hash=EXCLUDED.token_hash, \
         requested_at=EXCLUDED.requested_at,expires_at=EXCLUDED.expires_at,original_email=EXCLUDED.original_email, \
         security_version=EXCLUDED.security_version,factor_fingerprint=EXCLUDED.factor_fingerprint",
    )
    .bind(user_id)
    .bind(normalized_email)
    .bind(token_hash.as_slice())
    .bind(now)
    .bind(expires_at)
    .bind(original_email)
    .bind(security_version)
    .bind(factor_ciphertext.map(|bytes| Sha256::digest(bytes).to_vec()))
    .execute(&mut **tx)
    .await?;
    record_in(
        tx,
        &AuditCtx::default(),
        &NewEvent::new("account.email_change_requested").target("user", user_id),
    )
    .await?;
    Ok(true)
}

/// Applies a pending address only after a valid, unexpired, single-use proof.
pub async fn confirm_email_change(
    pool: &PgPool,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
) -> Result<ConfirmEmailChangeOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM pending_email_changes WHERE token_hash=$1")
            .bind(token_hash.as_slice())
            .fetch_optional(&mut *tx)
            .await?;
    let Some(user_id) = owner else {
        return Ok(ConfirmEmailChangeOutcome::Invalid);
    };
    sqlx::query("SELECT 1 FROM users WHERE id=$1 AND disabled_at IS NULL FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let target: Option<(String, String, i64, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT normalized_email,original_email,security_version,factor_fingerprint FROM pending_email_changes \
         WHERE user_id=$1 AND token_hash=$2 AND expires_at>$3",
    )
    .bind(user_id)
    .bind(token_hash.as_slice())
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((normalized_email, original_email, security_version, factor_fingerprint)) = target
    else {
        return Ok(ConfirmEmailChangeOutcome::Invalid);
    };
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{}:{normalized_email}", normalized_email.len()))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM pending_email_changes WHERE normalized_email=$1 AND user_id<>$2 AND expires_at<=$3")
        .bind(&normalized_email)
        .bind(user_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let occupied: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE normalized_email=$1 AND id<>$2) \
         OR EXISTS(SELECT 1 FROM pending_email_changes WHERE normalized_email=$1 AND user_id<>$2)",
    )
    .bind(&normalized_email)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if occupied {
        sqlx::query("DELETE FROM pending_email_changes WHERE user_id=$1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(ConfirmEmailChangeOutcome::Invalid);
    }
    let current: Option<(String, i64, String, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT normalized_email,security_version,display_name,(SELECT secret_ciphertext FROM user_two_factor \
         WHERE user_id=users.id AND enabled_at IS NOT NULL) FROM users WHERE id=$1 AND disabled_at IS NULL",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((current_email, current_version, display_name, current_factor)) = current else {
        return Ok(ConfirmEmailChangeOutcome::Invalid);
    };
    if current_email != original_email
        || current_version != security_version
        || factor_fingerprint != current_factor.map(|bytes| Sha256::digest(bytes).to_vec())
    {
        sqlx::query("DELETE FROM pending_email_changes WHERE user_id=$1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(ConfirmEmailChangeOutcome::Invalid);
    }
    let changed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE users SET normalized_email=$2,email_verified_at=$3,security_version=security_version+1 \
         WHERE id=$1 AND disabled_at IS NULL AND NOT EXISTS( \
             SELECT 1 FROM users other WHERE other.normalized_email=$2 AND other.id<>$1) \
         RETURNING id",
    )
    .bind(user_id)
    .bind(normalized_email)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user_id) = changed else {
        return Ok(ConfirmEmailChangeOutcome::Invalid);
    };
    sqlx::query("DELETE FROM two_factor_login_challenges WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE sessions SET revoked_at=$2 WHERE user_id=$1 AND revoked_at IS NULL")
        .bind(user_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    crate::locks::delete_for_user_tx(
        &mut tx,
        user_id,
        crate::locks::LockSessionKind::Browser,
        None,
    )
    .await?;
    sqlx::query("UPDATE password_reset_tokens SET used_at=$2 WHERE user_id=$1 AND used_at IS NULL")
        .bind(user_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let audit = AuditCtx {
        actor_id: Some(user_id),
        request_id: None,
    };
    crate::device_sessions::revoke_all_for_user_tx(&mut tx, user_id, now, &audit).await?;
    let notice = crate::email_outbox::EmailDraft {
        id: Uuid::new_v4(),
        recipient: original_email,
        template: "security_notice".to_owned(),
        language: "en".to_owned(),
        params: serde_json::json!({
            "recipient_name": display_name,
            "security_event": "email address changed"
        }),
        encrypted_token: None,
        expires_at: now + time::Duration::hours(24),
        actor_id: Some(user_id),
        is_resend: false,
        now,
    };
    if crate::email_outbox::enqueue_in(&mut tx, &notice).await?
        != crate::email_outbox::EnqueueOutcome::Queued
    {
        tx.rollback().await?;
        return Ok(ConfirmEmailChangeOutcome::RateLimited);
    }
    record_in(
        &mut tx,
        &AuditCtx::default(),
        &NewEvent::new("account.email_changed").target("user", user_id),
    )
    .await?;
    sqlx::query("DELETE FROM pending_email_changes WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM email_verification_tokens WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ConfirmEmailChangeOutcome::Confirmed)
}
