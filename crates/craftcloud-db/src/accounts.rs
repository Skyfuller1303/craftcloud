//! Account lookups and credential updates for login, password change and
//! recovery. Password hashes are only ever read for verification and never
//! returned in general-purpose types.

use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;

/// What login needs to know about an account.
#[derive(Debug, Clone, FromRow)]
pub struct LoginRecord {
    /// Account identifier.
    pub user_id: Uuid,
    /// Stored Argon2id PHC string; `None` for accounts without a password.
    pub password_hash: Option<String>,
    /// Whether the account is disabled.
    pub disabled: bool,
}

/// Public account data (no credentials).
#[derive(Debug, Clone, FromRow)]
pub struct Account {
    /// Account identifier.
    pub id: Uuid,
    /// Normalized e-mail address.
    pub normalized_email: String,
    /// Display name.
    pub display_name: String,
    /// Instance role (`operator`) if any.
    pub instance_role: Option<String>,
    /// Time the account was disabled, if it is.
    pub disabled_at: Option<OffsetDateTime>,
}

/// Looks up the login record for a normalized e-mail address.
///
/// # Errors
///
/// Database failures only.
pub async fn find_login(
    pool: &PgPool,
    normalized_email: &str,
) -> Result<Option<LoginRecord>, DbError> {
    Ok(sqlx::query_as::<_, LoginRecord>(
        "SELECT id AS user_id, password_hash, (disabled_at IS NOT NULL) AS disabled \
         FROM users WHERE normalized_email = $1",
    )
    .bind(normalized_email)
    .fetch_optional(pool)
    .await?)
}

/// Loads an account by id.
///
/// # Errors
///
/// Database failures only.
pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Account>, DbError> {
    Ok(sqlx::query_as::<_, Account>(
        "SELECT id, normalized_email, display_name, instance_role, disabled_at \
         FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Loads an account by normalized e-mail address.
///
/// # Errors
///
/// Database failures only.
pub async fn find_by_email(
    pool: &PgPool,
    normalized_email: &str,
) -> Result<Option<Account>, DbError> {
    Ok(sqlx::query_as::<_, Account>(
        "SELECT id, normalized_email, display_name, instance_role, disabled_at \
         FROM users WHERE normalized_email = $1",
    )
    .bind(normalized_email)
    .fetch_optional(pool)
    .await?)
}

/// Whether `user_id` is the instance operator recorded by the one-time setup.
///
/// # Errors
///
/// Database failures only.
pub async fn is_setup_operator(pool: &PgPool, user_id: Uuid) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM instance_setup WHERE operator_user_id = $1)",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?)
}

/// The stored password hash of an account (for verifying the current
/// password); `None` if the account does not exist or has no password.
///
/// # Errors
///
/// Database failures only.
pub async fn password_hash(pool: &PgPool, user_id: Uuid) -> Result<Option<String>, DbError> {
    let hash: Option<Option<String>> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(hash.flatten())
}

/// Replaces the stored hash with an upgraded one, only if the stored hash is
/// still `old_hash` (compare-and-set, so a concurrent password change wins).
/// Returns whether the row was updated.
///
/// # Errors
///
/// Database failures only.
pub async fn rehash_password(
    pool: &PgPool,
    user_id: Uuid,
    old_hash: &str,
    new_hash: &str,
) -> Result<bool, DbError> {
    let result =
        sqlx::query("UPDATE users SET password_hash = $3 WHERE id = $1 AND password_hash = $2")
            .bind(user_id)
            .bind(old_hash)
            .bind(new_hash)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() == 1)
}

/// Changes the password of a user who proved knowledge of the current one.
///
/// All checks and writes happen in one transaction, users row first and
/// sessions second (the same lock order as reset and link issuing, so they
/// cannot deadlock):
///
/// 1. the stored hash must still be `verified_hash` (a reset or another
///    change that committed meanwhile makes this fail);
/// 2. the session `keep_session` of this user must still be active (a link
///    issuance or logout that revoked it meanwhile makes this fail).
///
/// Then the new hash is stored, every other session of the user is revoked
/// and pending reset tokens are voided. Returns the number of revoked
/// sessions, or `None` (nothing changed) if one of the checks failed.
///
/// # Errors
///
/// Database failures only; nothing changes on failure.
pub async fn change_password(
    pool: &PgPool,
    user_id: Uuid,
    verified_hash: &str,
    new_hash: &str,
    keep_session: Uuid,
    now: OffsetDateTime,
) -> Result<Option<u64>, DbError> {
    let mut tx = pool.begin().await?;
    let updated =
        sqlx::query("UPDATE users SET password_hash = $3 WHERE id = $1 AND password_hash = $2")
            .bind(user_id)
            .bind(verified_hash)
            .bind(new_hash)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if updated != 1 {
        return Ok(None);
    }
    let session_alive: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM sessions WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL FOR UPDATE",
    )
    .bind(keep_session)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if session_alive.is_none() {
        return Ok(None);
    }
    let revoked = sqlx::query(
        "UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL AND id <> $3",
    )
    .bind(user_id)
    .bind(now)
    .bind(keep_session)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // A pending reset link must not outlive the password change.
    sqlx::query(
        "UPDATE password_reset_tokens SET used_at = $2 WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(revoked))
}
