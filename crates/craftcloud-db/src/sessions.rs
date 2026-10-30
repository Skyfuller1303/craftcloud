//! Server-side web sessions. Only the SHA-256 hash of the session token is
//! stored. All time-dependent functions take the current time explicitly so
//! tests can control the clock.

use std::time::Duration;

use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, Detail, NewEvent, record_in};
use crate::device_sessions;
use crate::locks::{self, LockSessionKind};

/// Input for [`create`].
#[derive(Debug, Clone)]
pub struct NewSession {
    /// Owner of the session.
    pub user_id: Uuid,
    /// SHA-256 of the session token.
    pub token_hash: [u8; 32],
    /// Creation time.
    pub now: OffsetDateTime,
    /// Hard expiry independent of activity.
    pub absolute_expires_at: OffsetDateTime,
}

/// An active session.
#[derive(Debug, Clone, FromRow)]
pub struct Session {
    /// Session identifier.
    pub id: Uuid,
    /// Owner.
    pub user_id: Uuid,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last recorded activity.
    pub last_seen_at: OffsetDateTime,
    /// Hard expiry.
    pub absolute_expires_at: OffsetDateTime,
}

/// Creates a session for a login that was verified against `verified_hash`,
/// but only if the account still has exactly that password hash and is not
/// disabled at the moment of the insert. The check and the insert are one
/// statement, and the user row is share-locked while it runs, so a password
/// change or reset that commits between the verification and this call (or
/// concurrently with it) cannot leave behind a session for the old password:
/// either this returns `None`, or the concurrent change waits for the insert
/// and then revokes the new session as part of its own transaction.
///
/// # Errors
///
/// Database failures only.
pub async fn create_for_verified_login(
    pool: &PgPool,
    new: &NewSession,
    verified_hash: &str,
) -> Result<Option<Uuid>, DbError> {
    create_for_verified_login_inner(pool, new, verified_hash, None).await
}

/// Like [`create_for_verified_login`], and records the `auth.login` audit
/// event (actor: the user) in the same transaction as the session.
///
/// # Errors
///
/// Database failures; a failing audit insert fails the login.
pub async fn create_for_verified_login_audited(
    pool: &PgPool,
    new: &NewSession,
    verified_hash: &str,
    audit: &AuditCtx,
) -> Result<Option<Uuid>, DbError> {
    create_for_verified_login_inner(pool, new, verified_hash, Some(audit)).await
}

async fn create_for_verified_login_inner(
    pool: &PgPool,
    new: &NewSession,
    verified_hash: &str,
    audit: Option<&AuditCtx>,
) -> Result<Option<Uuid>, DbError> {
    let id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    let user: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id=$1 AND password_hash=$2 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(new.user_id)
    .bind(verified_hash)
    .fetch_optional(&mut *tx)
    .await?;
    if user.is_none() {
        return Ok(None);
    }
    let factor_enabled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)",
    )
    .bind(new.user_id)
    .fetch_one(&mut *tx)
    .await?;
    if factor_enabled {
        return Ok(None);
    }
    let result = sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, created_at, last_seen_at, absolute_expires_at) \
         SELECT $1, $2, $3, $4, $4, $5 \
         WHERE EXISTS (SELECT 1 FROM users \
                       WHERE id = $2 AND password_hash = $6 AND disabled_at IS NULL FOR SHARE)",
    )
    .bind(id)
    .bind(new.user_id)
    .bind(new.token_hash.as_slice())
    .bind(new.now)
    .bind(new.absolute_expires_at)
    .bind(verified_hash)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() != 1 {
        return Ok(None);
    }
    if let Some(audit) = audit {
        let by = AuditCtx {
            actor_id: Some(new.user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.login").target("user", new.user_id),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Some(id))
}

/// Finds the active session for a token hash: not revoked, not past its
/// absolute expiry, seen within `idle_timeout`, and owned by an enabled
/// account.
///
/// # Errors
///
/// Database failures only.
pub async fn find_active(
    pool: &PgPool,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
    idle_timeout: Duration,
) -> Result<Option<Session>, DbError> {
    let idle_cutoff = now - idle_timeout;
    Ok(sqlx::query_as::<_, Session>(
        "SELECT s.id, s.user_id, s.created_at, s.last_seen_at, s.absolute_expires_at \
         FROM sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.token_hash = $1 AND s.revoked_at IS NULL \
           AND s.absolute_expires_at > $2 AND s.last_seen_at > $3 \
           AND u.disabled_at IS NULL",
    )
    .bind(token_hash.as_slice())
    .bind(now)
    .bind(idle_cutoff)
    .fetch_optional(pool)
    .await?)
}

/// Records activity, but only if the last recorded activity is at least
/// `min_interval` old (throttles write load). Returns whether it wrote.
///
/// # Errors
///
/// Database failures only.
pub async fn touch(
    pool: &PgPool,
    id: Uuid,
    now: OffsetDateTime,
    min_interval: Duration,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        "UPDATE sessions SET last_seen_at = $2 \
         WHERE id = $1 AND revoked_at IS NULL AND last_seen_at <= $3",
    )
    .bind(id)
    .bind(now)
    .bind(now - min_interval)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Revokes one session. Returns whether it was active.
///
/// # Errors
///
/// Database failures only.
pub async fn revoke(pool: &PgPool, id: Uuid, now: OffsetDateTime) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let revoked =
        sqlx::query("UPDATE sessions SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL")
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
    if revoked {
        locks::delete_for_session_tx(&mut tx, id).await?;
    }
    tx.commit().await?;
    Ok(revoked)
}

/// Revokes the session with the given token hash, if any (used when a login
/// replaces a session presented with the request). Returns whether one was
/// active.
///
/// # Errors
///
/// Database failures only.
pub async fn revoke_by_token_hash(
    pool: &PgPool,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "UPDATE sessions SET revoked_at = $2 WHERE token_hash = $1 AND revoked_at IS NULL RETURNING id",
    )
    .bind(token_hash.as_slice())
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = id {
        locks::delete_for_session_tx(&mut tx, id).await?;
    }
    tx.commit().await?;
    Ok(id.is_some())
}

/// Revokes a session only if it belongs to `user_id`. Another user's or an
/// unknown session id changes nothing and returns `false` (callers answer
/// 404, indistinguishable).
///
/// # Errors
///
/// Database failures only.
pub async fn revoke_owned(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let revoked = sqlx::query(
        "UPDATE sessions SET revoked_at = $3 WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if revoked {
        locks::delete_for_session_tx(&mut tx, id).await?;
    }
    tx.commit().await?;
    Ok(revoked)
}

/// Like [`revoke_owned`], and records `auth.session_revoke` (actor: the user)
/// in the same transaction when a session was revoked. The caller passes
/// `record = false` for the session in use (an ordinary logout is not an
/// audit event).
///
/// # Errors
///
/// Database failures; a failing audit insert leaves the session active.
pub async fn revoke_owned_audited(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    now: OffsetDateTime,
    audit: &AuditCtx,
    record: bool,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let revoked = sqlx::query(
        "UPDATE sessions SET revoked_at = $3 WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if revoked {
        locks::delete_for_session_tx(&mut tx, id).await?;
    }
    if revoked && record {
        let by = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.session_revoke").target("session", id),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(revoked)
}

/// Maximum number of sessions returned by [`list_active_for_user`].
pub const MAX_LISTED_SESSIONS: i64 = 100;

/// Lists the active (not revoked, not expired) sessions of a user, newest
/// first, at most [`MAX_LISTED_SESSIONS`]. Contains no network data.
///
/// # Errors
///
/// Database failures only.
pub async fn list_active_for_user(
    pool: &PgPool,
    user_id: Uuid,
    now: OffsetDateTime,
    idle_timeout: Duration,
) -> Result<Vec<Session>, DbError> {
    Ok(sqlx::query_as::<_, Session>(
        "SELECT id, user_id, created_at, last_seen_at, absolute_expires_at FROM sessions \
         WHERE user_id = $1 AND revoked_at IS NULL AND absolute_expires_at > $2 AND last_seen_at > $3 \
         ORDER BY created_at DESC, id LIMIT $4",
    )
    .bind(user_id)
    .bind(now)
    .bind(now - idle_timeout)
    .bind(MAX_LISTED_SESSIONS)
    .fetch_all(pool)
    .await?)
}

/// Revokes every active session of a user. Returns the number revoked.
///
/// # Errors
///
/// Database failures only.
pub async fn revoke_all_for_user(
    pool: &PgPool,
    user_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let result =
        sqlx::query("UPDATE sessions SET revoked_at=$2 WHERE user_id=$1 AND revoked_at IS NULL")
            .bind(user_id)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    locks::delete_for_user_tx(&mut tx, user_id, LockSessionKind::Browser, None).await?;
    let audit = AuditCtx {
        actor_id: Some(user_id),
        request_id: None,
    };
    device_sessions::revoke_all_for_user_tx(&mut tx, user_id, now, &audit).await?;
    tx.commit().await?;
    Ok(result)
}

/// Like [`revoke_all_for_user`], and records the `auth.logout_all` audit event
/// (actor: the user) in the same transaction.
///
/// # Errors
///
/// Database failures; a failing audit insert leaves the sessions active.
pub async fn revoke_all_for_user_audited(
    pool: &PgPool,
    user_id: Uuid,
    now: OffsetDateTime,
    audit: &AuditCtx,
) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let revoked = sqlx::query(
        "UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    locks::delete_for_user_tx(&mut tx, user_id, LockSessionKind::Browser, None).await?;
    let device_audit = AuditCtx {
        actor_id: Some(user_id),
        request_id: audit.request_id.clone(),
    };
    device_sessions::revoke_all_for_user_tx(&mut tx, user_id, now, &device_audit).await?;
    let by = AuditCtx {
        actor_id: Some(user_id),
        request_id: audit.request_id.clone(),
    };
    record_in(
        &mut tx,
        &by,
        &NewEvent::new("auth.logout_all")
            .target("user", user_id)
            .detail("sessions", Detail::Count(count(revoked))),
    )
    .await?;
    tx.commit().await?;
    Ok(revoked)
}

fn count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Deletes sessions that expired or were revoked before `cutoff`. Returns the
/// number of deleted rows.
///
/// # Errors
///
/// Database failures only.
pub async fn delete_expired(pool: &PgPool, cutoff: OffsetDateTime) -> Result<u64, DbError> {
    let result = sqlx::query(
        "DELETE FROM sessions \
         WHERE absolute_expires_at < $1 OR (revoked_at IS NOT NULL AND revoked_at < $1)",
    )
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
