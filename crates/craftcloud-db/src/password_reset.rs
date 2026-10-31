//! Single-use password reset tokens.
//!
//! **Global lock order** for every multi-row transaction touching accounts
//! (`create`, `consume`, `accounts::change_password`): the `users` row first,
//! then `sessions` and `password_reset_tokens`. Single-statement writes
//! (logout, revoke, void) lock only rows of one table and cannot take part in
//! a cycle. Following this order makes deadlocks (SQLSTATE 40P01) between
//! these paths impossible, so no retry logic is needed. Only the SHA-256 hash of the token is
//! stored; time is passed in explicitly.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, Detail, NewEvent, record_in};
use crate::device_sessions;

/// Input for [`create`].
#[derive(Debug, Clone)]
pub struct NewResetToken {
    /// Account the reset is for.
    pub user_id: Uuid,
    /// SHA-256 of the token.
    pub token_hash: [u8; 32],
    /// Operator who issued it; `None` for the offline CLI.
    pub created_by: Option<Uuid>,
    /// Whether issuing the link also resets the account's second factor.
    pub reset_two_factor: bool,
    /// Issue time.
    pub now: OffsetDateTime,
    /// Expiry time.
    pub expires_at: OffsetDateTime,
}

/// Account a valid token belongs to.
#[derive(Debug, Clone)]
pub struct ResetTarget {
    /// Account identifier.
    pub user_id: Uuid,
    /// Normalized e-mail address (needed for the password policy).
    pub normalized_email: String,
}

/// Result of [`create`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueOutcome {
    /// The token was stored; this many active sessions of the user were
    /// revoked (ADR-0013: issuing a link is a compromise response).
    Issued {
        /// Number of sessions revoked.
        revoked_sessions: u64,
    },
    /// The account is disabled; nothing was stored.
    AccountDisabled,
    /// The account does not exist; nothing was stored.
    NoSuchUser,
    /// Durable email limits refused atomic link delivery.
    EmailRateLimited,
}

/// Issues a reset token in one transaction: older pending tokens of the user
/// are voided, every active session of the user is revoked, and the new token
/// is stored. Disabled or unknown accounts get nothing.
///
/// # Errors
///
/// Database failures only; nothing changes on failure.
pub async fn create(pool: &PgPool, new: &NewResetToken) -> Result<IssueOutcome, DbError> {
    create_inner(pool, new, None, None).await
}

/// Like [`create`], and records the `password_reset.issue` audit event (actor
/// from `audit`, normally the operator) in the same transaction. The event
/// never contains the token.
///
/// # Errors
///
/// Database failures; a failing audit insert stores no token.
pub async fn create_audited(
    pool: &PgPool,
    new: &NewResetToken,
    audit: &AuditCtx,
) -> Result<IssueOutcome, DbError> {
    create_inner(pool, new, Some(audit), None).await
}

/// Like [`create_audited`], with an email inserted in the same transaction.
pub async fn create_audited_with_email(
    pool: &PgPool,
    new: &NewResetToken,
    audit: &AuditCtx,
    draft: &crate::email_outbox::EmailDraft,
) -> Result<IssueOutcome, DbError> {
    create_inner(pool, new, Some(audit), Some(draft)).await
}

async fn create_inner(
    pool: &PgPool,
    new: &NewResetToken,
    audit: Option<&AuditCtx>,
    email: Option<&crate::email_outbox::EmailDraft>,
) -> Result<IssueOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let disabled: Option<bool> =
        sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM users WHERE id = $1 FOR UPDATE")
            .bind(new.user_id)
            .fetch_optional(&mut *tx)
            .await?;
    match disabled {
        None => return Ok(IssueOutcome::NoSuchUser),
        Some(true) => return Ok(IssueOutcome::AccountDisabled),
        Some(false) => {}
    }
    // Reset issuance is already a compromise-response boundary (it revokes
    // sessions). Invalidate previously authorized address changes immediately,
    // without waiting for the recipient to consume the reset link.
    sqlx::query("UPDATE users SET security_version=security_version+1 WHERE id=$1")
        .bind(new.user_id)
        .execute(&mut *tx)
        .await?;
    if new.reset_two_factor {
        let factor_removed = sqlx::query("DELETE FROM user_two_factor WHERE user_id=$1")
            .bind(new.user_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if factor_removed > 0
            && email.is_some()
            && crate::mail_security::enqueue_in(
                &mut tx,
                new.user_id,
                new.now,
                crate::mail_security::SecurityEvent::TwoFactorReset,
            )
            .await?
                != crate::email_outbox::EnqueueOutcome::Queued
        {
            tx.rollback().await?;
            return Ok(IssueOutcome::EmailRateLimited);
        }
    }
    sqlx::query("DELETE FROM two_factor_login_challenges WHERE user_id=$1")
        .bind(new.user_id)
        .execute(&mut *tx)
        .await?;
    // The user row is locked above, so the outbox FK locks follow the documented order.
    crate::figcraft::enqueue_all_user_revocations(
        &mut tx,
        new.user_id,
        "security_changed",
        new.now,
    )
    .await?;
    sqlx::query(
        "UPDATE password_reset_tokens SET used_at = $2 WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(new.user_id)
    .bind(new.now)
    .execute(&mut *tx)
    .await?;
    if let Some(draft) = email {
        let queued = crate::email_outbox::enqueue_in(&mut tx, draft).await?;
        if queued != crate::email_outbox::EnqueueOutcome::Queued {
            tx.rollback().await?;
            return Ok(IssueOutcome::EmailRateLimited);
        }
    }
    let revoked_sessions = sqlx::query(
        "UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(new.user_id)
    .bind(new.now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    crate::locks::delete_for_user_tx(
        &mut tx,
        new.user_id,
        crate::locks::LockSessionKind::Browser,
        None,
    )
    .await?;
    let device_audit = audit.cloned().unwrap_or(AuditCtx {
        actor_id: Some(new.user_id),
        request_id: None,
    });
    device_sessions::revoke_all_for_user_tx(&mut tx, new.user_id, new.now, &device_audit).await?;
    sqlx::query(
        "INSERT INTO password_reset_tokens (id, user_id, token_hash, created_by_user_id, created_at, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(new.user_id)
    .bind(new.token_hash.as_slice())
    .bind(new.created_by)
    .bind(new.now)
    .bind(new.expires_at)
    .execute(&mut *tx)
    .await?;
    if let Some(audit) = audit {
        record_in(
            &mut tx,
            audit,
            &NewEvent::new("password_reset.issue")
                .target("user", new.user_id)
                .detail(
                    "sessions_revoked",
                    Detail::Count(i64::try_from(revoked_sessions).unwrap_or(i64::MAX)),
                ),
        )
        .await?;
        if new.reset_two_factor {
            record_in(
                &mut tx,
                audit,
                &NewEvent::new("auth.two_factor_reset").target("user", new.user_id),
            )
            .await?;
        }
    }
    tx.commit().await?;
    Ok(IssueOutcome::Issued { revoked_sessions })
}

/// Voids every pending (unused) reset token of a user without touching
/// sessions. Returns the number of voided tokens.
///
/// # Errors
///
/// Database failures only.
pub async fn void_pending(
    pool: &PgPool,
    user_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DbError> {
    let result = sqlx::query(
        "UPDATE password_reset_tokens SET used_at = $2 WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Like [`void_pending`], and records `password_reset.void` (actor from
/// `audit`) in the same transaction when at least one link was voided.
///
/// # Errors
///
/// Database failures; a failing audit insert voids nothing.
pub async fn void_pending_audited(
    pool: &PgPool,
    user_id: Uuid,
    now: OffsetDateTime,
    audit: &AuditCtx,
) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    let voided = sqlx::query(
        "UPDATE password_reset_tokens SET used_at = $2 WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if voided > 0 {
        record_in(
            &mut tx,
            audit,
            &NewEvent::new("password_reset.void")
                .target("user", user_id)
                .detail(
                    "links",
                    Detail::Count(i64::try_from(voided).unwrap_or(i64::MAX)),
                ),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(voided)
}

/// Returns the account for a token that is unused and not expired, without
/// consuming it.
///
/// # Errors
///
/// Database failures only.
pub async fn peek(
    pool: &PgPool,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
) -> Result<Option<ResetTarget>, DbError> {
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT u.id, u.normalized_email FROM password_reset_tokens t \
         JOIN users u ON u.id = t.user_id \
         WHERE t.token_hash = $1 AND t.used_at IS NULL AND t.expires_at > $2 \
           AND u.disabled_at IS NULL",
    )
    .bind(token_hash.as_slice())
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(user_id, normalized_email)| ResetTarget {
        user_id,
        normalized_email,
    }))
}

/// Atomically consumes the token, sets the new password hash and revokes all
/// sessions of the user (and all other pending reset tokens of that user).
/// Returns the user id, or `None` if the token is unknown, used or expired;
/// nothing changes in that case.
///
/// # Errors
///
/// Database failures only; nothing changes on failure.
pub async fn consume(
    pool: &PgPool,
    token_hash: &[u8; 32],
    new_password_hash: &str,
    now: OffsetDateTime,
) -> Result<Option<Uuid>, DbError> {
    Ok(
        consume_inner(pool, token_hash, new_password_hash, now, None, None)
            .await?
            .user_id(),
    )
}

/// Like [`consume`], and records the `password_reset.use` audit event (actor:
/// the account whose password was reset) in the same transaction. A token
/// that cannot be consumed (unknown, used, expired) records nothing, so a
/// replay never produces a second event.
///
/// # Errors
///
/// Database failures; a failing audit insert leaves the token unused.
pub async fn consume_audited(
    pool: &PgPool,
    token_hash: &[u8; 32],
    new_password_hash: &str,
    now: OffsetDateTime,
    audit: &AuditCtx,
) -> Result<Option<Uuid>, DbError> {
    Ok(
        consume_inner(pool, token_hash, new_password_hash, now, Some(audit), None)
            .await?
            .user_id(),
    )
}

/// Like [`consume_audited`], and atomically queues a post-reset notice when enabled.
pub async fn consume_audited_with_email(
    pool: &PgPool,
    token_hash: &[u8; 32],
    new_password_hash: &str,
    now: OffsetDateTime,
    audit: &AuditCtx,
    mail_enabled: bool,
) -> Result<ConsumeOutcome, DbError> {
    consume_inner(
        pool,
        token_hash,
        new_password_hash,
        now,
        Some(audit),
        mail_enabled.then_some(crate::mail_security::SecurityEvent::PasswordReset),
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeOutcome {
    Consumed(Uuid),
    InvalidToken,
    EmailRateLimited,
}

impl ConsumeOutcome {
    fn user_id(self) -> Option<Uuid> {
        match self {
            Self::Consumed(user_id) => Some(user_id),
            Self::InvalidToken | Self::EmailRateLimited => None,
        }
    }
}

async fn consume_inner(
    pool: &PgPool,
    token_hash: &[u8; 32],
    new_password_hash: &str,
    now: OffsetDateTime,
    audit: Option<&AuditCtx>,
    email_event: Option<crate::mail_security::SecurityEvent>,
) -> Result<ConsumeOutcome, DbError> {
    let mut tx = pool.begin().await?;
    // Lock order: the users row first (like `create` and
    // `accounts::change_password`), then tokens and sessions. The token's
    // owner is read without a lock, the user row is locked, and only then is
    // the token claimed with all its conditions re-checked.
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM password_reset_tokens WHERE token_hash = $1")
            .bind(token_hash.as_slice())
            .fetch_optional(&mut *tx)
            .await?;
    let Some(owner) = owner else {
        return Ok(ConsumeOutcome::InvalidToken);
    };
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    let user_id: Option<Uuid> = sqlx::query_scalar(
        "UPDATE password_reset_tokens SET used_at = $2 \
         WHERE token_hash = $1 AND user_id = $3 AND used_at IS NULL AND expires_at > $2 \
           AND EXISTS (SELECT 1 FROM users u WHERE u.id = $3 AND u.disabled_at IS NULL) \
         RETURNING user_id",
    )
    .bind(token_hash.as_slice())
    .bind(now)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user_id) = user_id else {
        return Ok(ConsumeOutcome::InvalidToken);
    };
    sqlx::query(
        "UPDATE users SET password_hash = $2, security_version=security_version+1 WHERE id = $1",
    )
    .bind(user_id)
    .bind(new_password_hash)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM two_factor_login_challenges WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    crate::figcraft::enqueue_all_user_revocations(&mut tx, user_id, "security_changed", now)
        .await?;
    sqlx::query("UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL")
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
    sqlx::query(
        "UPDATE password_reset_tokens SET used_at = $2 WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let device_audit = AuditCtx {
        actor_id: Some(user_id),
        request_id: audit.and_then(|ctx| ctx.request_id.clone()),
    };
    device_sessions::revoke_all_for_user_tx(&mut tx, user_id, now, &device_audit).await?;
    if let Some(audit) = audit {
        let by = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("password_reset.use").target("user", user_id),
        )
        .await?;
    }
    if let Some(event) = email_event
        && crate::mail_security::enqueue_in(&mut tx, user_id, now, event).await?
            != crate::email_outbox::EnqueueOutcome::Queued
    {
        tx.rollback().await?;
        return Ok(ConsumeOutcome::EmailRateLimited);
    }
    tx.commit().await?;
    Ok(ConsumeOutcome::Consumed(user_id))
}

/// Deletes reset tokens that expired or were used before `cutoff`.
///
/// # Errors
///
/// Database failures only.
pub async fn delete_expired(pool: &PgPool, cutoff: OffsetDateTime) -> Result<u64, DbError> {
    let result = sqlx::query(
        "DELETE FROM password_reset_tokens \
         WHERE expires_at < $1 OR (used_at IS NOT NULL AND used_at < $1)",
    )
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
