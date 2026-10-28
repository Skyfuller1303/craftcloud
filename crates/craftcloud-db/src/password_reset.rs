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

/// Input for [`create`].
#[derive(Debug, Clone)]
pub struct NewResetToken {
    /// Account the reset is for.
    pub user_id: Uuid,
    /// SHA-256 of the token.
    pub token_hash: [u8; 32],
    /// Operator who issued it; `None` for the offline CLI.
    pub created_by: Option<Uuid>,
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
}

/// Issues a reset token in one transaction: older pending tokens of the user
/// are voided, every active session of the user is revoked, and the new token
/// is stored. Disabled or unknown accounts get nothing.
///
/// # Errors
///
/// Database failures only; nothing changes on failure.
pub async fn create(pool: &PgPool, new: &NewResetToken) -> Result<IssueOutcome, DbError> {
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
    sqlx::query(
        "UPDATE password_reset_tokens SET used_at = $2 WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(new.user_id)
    .bind(new.now)
    .execute(&mut *tx)
    .await?;
    let revoked_sessions = sqlx::query(
        "UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(new.user_id)
    .bind(new.now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
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
        return Ok(None);
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
        return Ok(None);
    };
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user_id)
        .bind(new_password_hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL")
        .bind(user_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE password_reset_tokens SET used_at = $2 WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(user_id))
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
