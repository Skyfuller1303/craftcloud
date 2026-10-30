//! Database operations for desktop device sessions. Raw tokens never enter this crate.

use std::time::Duration;

use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, NewEvent, record_in};

#[derive(Debug, Clone)]
pub struct NewDeviceSession {
    pub user_id: Uuid,
    pub name: String,
    pub client_kind: String,
    pub client_id: String,
    pub refresh_hash: [u8; 32],
    pub access_hash: [u8; 32],
    pub now: OffsetDateTime,
    pub access_expires_at: OffsetDateTime,
    pub idle_expires_at: OffsetDateTime,
    pub absolute_expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct DeviceSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub client_kind: String,
    pub client_id: String,
    pub created_at: OffsetDateTime,
    pub last_used_at: OffsetDateTime,
    pub idle_expires_at: OffsetDateTime,
    pub absolute_expires_at: OffsetDateTime,
    pub access_expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, FromRow)]
pub struct DeviceActor {
    pub id: Uuid,
    pub user_id: Uuid,
    pub created_at: OffsetDateTime,
    pub last_used_at: OffsetDateTime,
    pub idle_expires_at: OffsetDateTime,
    pub absolute_expires_at: OffsetDateTime,
    pub access_expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    Rotated,
    Reused,
    Invalid,
}

/// Create device credentials only if password verification is still current.
pub async fn create_for_verified_login(
    pool: &PgPool,
    new: &NewDeviceSession,
    verified_hash: &str,
    audit: &AuditCtx,
) -> Result<Option<Uuid>, DbError> {
    let mut tx = pool.begin().await?;
    let id = Uuid::new_v4();
    let family = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO device_sessions (id,user_id,name,client_kind,client_id,refresh_hash,refresh_family,access_hash,access_expires_at,created_at,last_used_at,idle_expires_at,absolute_expires_at) \
         SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10,$11,$12 \
         WHERE EXISTS (SELECT 1 FROM users WHERE id=$2 AND password_hash=$13 AND disabled_at IS NULL FOR SHARE)",
    )
    .bind(id).bind(new.user_id).bind(&new.name).bind(&new.client_kind).bind(&new.client_id)
    .bind(new.refresh_hash.as_slice()).bind(family).bind(new.access_hash.as_slice())
    .bind(new.access_expires_at).bind(new.now).bind(new.idle_expires_at).bind(new.absolute_expires_at)
    .bind(verified_hash).execute(&mut *tx).await?.rows_affected();
    if inserted != 1 {
        return Ok(None);
    }
    let by = AuditCtx {
        actor_id: Some(new.user_id),
        request_id: audit.request_id.clone(),
    };
    record_in(
        &mut tx,
        &by,
        &NewEvent::new("auth.device_login").target("device_session", id),
    )
    .await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// Find a live access token and opportunistically update device activity.
pub async fn find_active_access(
    pool: &PgPool,
    hash: &[u8; 32],
    now: OffsetDateTime,
    touch_interval: Duration,
    idle_timeout: Duration,
) -> Result<Option<DeviceActor>, DbError> {
    let cutoff = now - touch_interval;
    let row = sqlx::query_as::<_, DeviceActor>(
        "SELECT d.id,d.user_id,d.created_at,d.last_used_at,d.idle_expires_at,d.absolute_expires_at,d.access_expires_at \
         FROM device_sessions d JOIN users u ON u.id=d.user_id \
         WHERE d.access_hash=$1 AND d.revoked_at IS NULL AND d.access_expires_at>$2 \
           AND d.idle_expires_at>$2 AND d.absolute_expires_at>$2 AND u.disabled_at IS NULL",
    ).bind(hash.as_slice()).bind(now).fetch_optional(pool).await?;
    let Some(mut row) = row else {
        return Ok(None);
    };
    if row.last_used_at <= cutoff {
        let expires = (now + idle_timeout).min(row.absolute_expires_at);
        let touched: Option<(OffsetDateTime, OffsetDateTime)> = sqlx::query_as("UPDATE device_sessions SET last_used_at=$2,idle_expires_at=$3 WHERE id=$1 AND revoked_at IS NULL AND access_hash=$4 RETURNING last_used_at,idle_expires_at")
            .bind(row.id).bind(now).bind(expires).bind(hash.as_slice()).fetch_optional(pool).await?;
        if let Some((last_used_at, idle_expires_at)) = touched {
            row.last_used_at = last_used_at;
            row.idle_expires_at = idle_expires_at;
        } else {
            return Ok(None);
        }
    }
    Ok(Some(row))
}

/// Atomically rotate current refresh/access hashes; any historical hash revokes the session.
// Keep independently typed token hashes and lifetime settings explicit at this transaction boundary.
#[allow(clippy::too_many_arguments)]
pub async fn refresh(
    pool: &PgPool,
    old_hash: &[u8; 32],
    new_refresh_hash: &[u8; 32],
    new_access_hash: &[u8; 32],
    now: OffsetDateTime,
    access_ttl: Duration,
    idle_timeout: Duration,
    audit: &AuditCtx,
) -> Result<(RefreshOutcome, Option<DeviceSession>), DbError> {
    let mut tx = pool.begin().await?;
    let found: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id,user_id FROM device_sessions WHERE refresh_hash=$1 UNION ALL SELECT d.id,d.user_id FROM device_refresh_history h JOIN device_sessions d ON d.id=h.device_session_id WHERE h.refresh_hash=$1 LIMIT 1",
    ).bind(old_hash.as_slice()).fetch_optional(&mut *tx).await?;
    let Some((id, user_id)) = found else {
        return Ok((RefreshOutcome::Invalid, None));
    };
    let enabled: Option<bool> =
        sqlx::query_scalar("SELECT disabled_at IS NULL FROM users WHERE id=$1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if enabled != Some(true) {
        return Ok((RefreshOutcome::Invalid, None));
    }
    let row: Option<(bool, OffsetDateTime, OffsetDateTime)> = sqlx::query_as(
        "SELECT refresh_hash=$2, idle_expires_at, absolute_expires_at FROM device_sessions WHERE id=$1 AND revoked_at IS NULL FOR UPDATE",
    ).bind(id).bind(old_hash.as_slice()).fetch_optional(&mut *tx).await?;
    let Some((current, idle, absolute)) = row else {
        return Ok((RefreshOutcome::Invalid, None));
    };
    if now >= idle || now >= absolute {
        return Ok((RefreshOutcome::Invalid, None));
    }
    if !current {
        sqlx::query("UPDATE device_sessions SET revoked_at=$2,revoke_reason='refresh_reuse',access_hash=NULL,access_expires_at=NULL WHERE refresh_family=(SELECT refresh_family FROM device_sessions WHERE id=$1) AND revoked_at IS NULL")
            .bind(id).bind(now).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM file_locks WHERE holder_session_kind='device' AND holder_session_id IN (SELECT id FROM device_sessions WHERE refresh_family=(SELECT refresh_family FROM device_sessions WHERE id=$1))")
            .bind(id).execute(&mut *tx).await?;
        let by = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.device_refresh_reuse").target("device_session", id),
        )
        .await?;
        tx.commit().await?;
        return Ok((RefreshOutcome::Reused, None));
    }
    sqlx::query("INSERT INTO device_refresh_history(device_session_id,refresh_hash,rotated_at) VALUES($1,$2,$3)")
        .bind(id).bind(old_hash.as_slice()).bind(now).execute(&mut *tx).await?;
    let new_idle = (now + idle_timeout).min(absolute);
    let new_access = (now + access_ttl).min(absolute);
    sqlx::query("UPDATE device_sessions SET refresh_hash=$2,access_hash=$3,access_expires_at=$4,last_used_at=$5,idle_expires_at=$6 WHERE id=$1")
        .bind(id).bind(new_refresh_hash.as_slice()).bind(new_access_hash.as_slice()).bind(new_access).bind(now).bind(new_idle).execute(&mut *tx).await?;
    let row = sqlx::query_as::<_, DeviceSession>("SELECT id,user_id,name,client_kind,client_id,created_at,last_used_at,idle_expires_at,absolute_expires_at,access_expires_at FROM device_sessions WHERE id=$1").bind(id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok((RefreshOutcome::Rotated, Some(row)))
}

pub async fn list_active(
    pool: &PgPool,
    user_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<DeviceSession>, DbError> {
    Ok(sqlx::query_as::<_, DeviceSession>("SELECT id,user_id,name,client_kind,client_id,created_at,last_used_at,idle_expires_at,absolute_expires_at,access_expires_at FROM device_sessions WHERE user_id=$1 AND revoked_at IS NULL AND idle_expires_at>$2 AND absolute_expires_at>$2 ORDER BY created_at DESC,id LIMIT 100")
        .bind(user_id).bind(now).fetch_all(pool).await?)
}

pub async fn revoke_owned(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    now: OffsetDateTime,
    reason: &str,
    audit: &AuditCtx,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let changed=sqlx::query("UPDATE device_sessions SET revoked_at=$3,revoke_reason=$4,access_hash=NULL,access_expires_at=NULL WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL")
        .bind(id).bind(user_id).bind(now).bind(reason).execute(&mut *tx).await?.rows_affected()==1;
    if changed {
        crate::locks::delete_for_session_tx(&mut tx, id).await?;
        let by = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.device_revoked").target("device_session", id),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn revoke_current(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    now: OffsetDateTime,
    audit: &AuditCtx,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let changed=sqlx::query("UPDATE device_sessions SET revoked_at=$3,revoke_reason='logout',access_hash=NULL,access_expires_at=NULL WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL")
        .bind(id).bind(user_id).bind(now).execute(&mut *tx).await?.rows_affected()==1;
    if changed {
        crate::locks::delete_for_session_tx(&mut tx, id).await?;
        let by = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.device_logout").target("device_session", id),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn revoke_all_for_user_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    now: OffsetDateTime,
    audit: &AuditCtx,
) -> Result<u64, DbError> {
    let ids: Vec<Uuid> = sqlx::query_scalar("UPDATE device_sessions SET revoked_at=$2,revoke_reason='security',access_hash=NULL,access_expires_at=NULL WHERE user_id=$1 AND revoked_at IS NULL RETURNING id")
        .bind(user_id).bind(now).fetch_all(&mut **tx).await?;
    crate::locks::delete_for_user_tx(tx, user_id, crate::locks::LockSessionKind::Device, None)
        .await?;
    let by = AuditCtx {
        actor_id: audit.actor_id,
        request_id: audit.request_id.clone(),
    };
    for id in &ids {
        record_in(
            tx,
            &by,
            &NewEvent::new("auth.device_revoked").target("device_session", *id),
        )
        .await?;
    }
    Ok(ids.len() as u64)
}

pub async fn count_expired(pool: &PgPool) -> Result<u64, DbError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM device_sessions WHERE idle_expires_at < now() - interval '30 days' OR revoked_at < now() - interval '30 days'",
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(u64::MAX))
}

/// Deletes at most `batch_size` device sessions expired or revoked 30 days ago.
pub async fn delete_expired(pool: &PgPool, batch_size: i64) -> Result<u64, DbError> {
    let removed: i64 = sqlx::query_scalar(
        "WITH doomed AS (SELECT id FROM device_sessions WHERE idle_expires_at < now() - interval '30 days' OR revoked_at < now() - interval '30 days' ORDER BY id LIMIT $1), gone AS (DELETE FROM device_sessions d USING doomed x WHERE d.id=x.id RETURNING 1) SELECT count(*) FROM gone",
    )
    .bind(batch_size.max(1))
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(removed).unwrap_or(u64::MAX))
}
