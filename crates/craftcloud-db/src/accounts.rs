//! Account lookups and credential updates for login, password change and
//! recovery. Password hashes are only ever read for verification and never
//! returned in general-purpose types.

use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::organizations::{AfterKey, clamp_page};

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

/// Instance-level account metadata for the operator. Contains no credentials
/// and no organization data beyond the provenance of invitation-created
/// accounts.
#[derive(Debug, Clone, FromRow)]
pub struct AccountSummary {
    /// Account identifier.
    pub id: Uuid,
    /// Normalized e-mail address.
    pub normalized_email: String,
    /// Display name.
    pub display_name: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Time the account was disabled, if it is.
    pub disabled_at: Option<OffsetDateTime>,
    /// Time the address was verified; `NULL` until a verification flow exists.
    pub email_verified_at: Option<OffsetDateTime>,
    /// Organization of the invitation that created the account.
    pub origin_organization_id: Option<Uuid>,
    /// User who created that invitation.
    pub invited_by_user_id: Option<Uuid>,
    /// Invitation that created the account.
    pub invitation_id: Option<Uuid>,
}

/// Lists all accounts ordered by `(created_at, id)`, one page after `after`;
/// with `unverified_only` only accounts without a verified address.
/// Operator-only by policy.
///
/// # Errors
///
/// Database failures only.
pub async fn list_all(
    pool: &PgPool,
    limit: u32,
    after: Option<AfterKey>,
    unverified_only: bool,
) -> Result<Vec<AccountSummary>, DbError> {
    Ok(sqlx::query_as::<_, AccountSummary>(
        "SELECT u.id, u.normalized_email, u.display_name, u.created_at, u.disabled_at,                 u.email_verified_at, i.org_id AS origin_organization_id,                 i.created_by_user_id AS invited_by_user_id,                 u.created_via_invitation_id AS invitation_id          FROM users u LEFT JOIN invitations i ON i.id = u.created_via_invitation_id          WHERE ($4::boolean = false OR u.email_verified_at IS NULL)            AND ($1::timestamptz IS NULL OR (u.created_at, u.id) > ($1::timestamptz, $2::uuid))          ORDER BY u.created_at, u.id LIMIT $3",
    )
    .bind(after.map(|(at, _)| at))
    .bind(after.map(|(_, id)| id))
    .bind(clamp_page(limit))
    .bind(unverified_only)
    .fetch_all(pool)
    .await?)
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

/// Result of [`set_disabled`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisableOutcome {
    /// The state changed. `orphaned_organizations` lists the organizations the
    /// account was an owner of that now have no enabled owner (only when
    /// disabling).
    Changed {
        /// Organizations left without an enabled owner.
        orphaned_organizations: Vec<Uuid>,
    },
    /// The account already was in the requested state (the orphan list is
    /// computed for the current state when disabling).
    Unchanged {
        /// Organizations without an enabled owner among the account's own.
        orphaned_organizations: Vec<Uuid>,
    },
    /// No such account.
    NoSuchUser,
    /// The instance operator cannot be disabled.
    IsOperator,
}

/// Upper bound of the orphan list in a disable result.
const MAX_ORPHANS: i64 = 1000;

/// Disables or enables an account. Disabling revokes every session of the
/// account and voids its pending reset links in the same transaction (lock
/// order: users row first). The instance operator is never disabled.
///
/// Disabling is **not** blocked when the account is the last enabled owner of
/// organizations (a security response must not be blocked); those
/// organizations are returned so the caller can report them and the operator
/// can recover them with the offline `set-org-owner` tool.
///
/// # Errors
///
/// Database failures only; nothing changes on failure.
pub async fn set_disabled(
    pool: &PgPool,
    user_id: Uuid,
    disabled: bool,
    now: OffsetDateTime,
) -> Result<DisableOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT disabled_at IS NOT NULL, \
                EXISTS (SELECT 1 FROM instance_setup s WHERE s.operator_user_id = users.id) \
         FROM users WHERE id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((currently_disabled, is_operator)) = row else {
        return Ok(DisableOutcome::NoSuchUser);
    };
    if disabled && is_operator {
        return Ok(DisableOutcome::IsOperator);
    }
    let changed = currently_disabled != disabled;
    if changed {
        sqlx::query(
            "UPDATE users SET disabled_at = CASE WHEN $2 THEN $3::timestamptz ELSE NULL END WHERE id = $1",
        )
        .bind(user_id)
        .bind(disabled)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if disabled {
            sqlx::query(
                "UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL",
            )
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
        }
    }
    let orphaned_organizations: Vec<Uuid> = if disabled {
        // Organizations this account owns that have no enabled active owner
        // (this account counts as disabled: its row is updated above).
        sqlx::query_scalar(
            "SELECT m.org_id FROM organization_members m \
             WHERE m.user_id = $1 AND m.role = 'owner' AND m.status = 'active' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM organization_members o JOIN users u ON u.id = o.user_id \
                   WHERE o.org_id = m.org_id AND o.role = 'owner' AND o.status = 'active' \
                     AND u.disabled_at IS NULL) \
             ORDER BY m.org_id LIMIT $2",
        )
        .bind(user_id)
        .bind(MAX_ORPHANS)
        .fetch_all(&mut *tx)
        .await?
    } else {
        Vec::new()
    };
    tx.commit().await?;
    Ok(if changed {
        DisableOutcome::Changed {
            orphaned_organizations,
        }
    } else {
        DisableOutcome::Unchanged {
            orphaned_organizations,
        }
    })
}
