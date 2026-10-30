//! File editing leases. Expired leases are treated as free by callers.
use crate::members::OrgTx;
use crate::{DbError, PgPool};
use sqlx::FromRow;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub const LEASE_SECONDS: i64 = 300;

#[derive(Debug, Clone, Copy)]
pub enum LockSessionKind {
    Browser,
}

impl LockSessionKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LockSessionContext {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub expires_at: OffsetDateTime,
    pub idle_timeout_secs: i64,
    pub kind: LockSessionKind,
}

#[derive(Debug, Clone, FromRow)]
pub struct FileLock {
    pub file_id: Uuid,
    pub org_id: Uuid,
    pub holder_user_id: Uuid,
    pub holder_session_kind: String,
    pub holder_session_id: Uuid,
    pub client_id: String,
    pub acquired_at: OffsetDateTime,
    pub refreshed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub enum AcquireOutcome {
    Acquired {
        current: FileLock,
        replaced_session: bool,
        created: bool,
    },
    Locked {
        current: FileLock,
        same_user: bool,
    },
    SessionExpired,
}

pub async fn get(pool: &PgPool, org_id: Uuid, file_id: Uuid) -> Result<Option<FileLock>, DbError> {
    Ok(sqlx::query_as::<_, FileLock>(
        "SELECT l.file_id,l.org_id,l.holder_user_id,l.holder_session_kind,l.holder_session_id,l.client_id,l.acquired_at,l.refreshed_at,l.expires_at,u.display_name FROM file_locks l JOIN files f ON f.org_id=l.org_id AND f.id=l.file_id JOIN users u ON u.id=l.holder_user_id WHERE l.org_id=$1 AND l.file_id=$2 AND f.app <> 'figcraft' AND l.expires_at>CURRENT_TIMESTAMP",
    ).bind(org_id).bind(file_id).fetch_optional(pool).await?)
}

impl OrgTx {
    pub async fn file_lock(&mut self, file_id: Uuid) -> Result<Option<FileLock>, DbError> {
        Ok(sqlx::query_as::<_, FileLock>(
            "SELECT l.file_id,l.org_id,l.holder_user_id,l.holder_session_kind,l.holder_session_id,l.client_id,l.acquired_at,l.refreshed_at,l.expires_at,u.display_name FROM file_locks l JOIN files f ON f.org_id=l.org_id AND f.id=l.file_id JOIN users u ON u.id=l.holder_user_id WHERE l.org_id=$1 AND l.file_id=$2 AND f.app <> 'figcraft' AND l.expires_at>CURRENT_TIMESTAMP",
        )
        .bind(self.org_id)
        .bind(file_id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    pub async fn file_app(&mut self, file_id: Uuid) -> Result<Option<String>, DbError> {
        Ok(
            sqlx::query_scalar("SELECT app FROM files WHERE org_id=$1 AND id=$2")
                .bind(self.org_id)
                .bind(file_id)
                .fetch_optional(&mut *self.tx)
                .await?,
        )
    }

    pub async fn active_project_lock(
        &mut self,
        project_id: Uuid,
        current_session_id: Uuid,
    ) -> Result<Option<FileLock>, DbError> {
        Ok(sqlx::query_as::<_, FileLock>(
            "SELECT l.file_id,l.org_id,l.holder_user_id,l.holder_session_kind,l.holder_session_id,l.client_id,l.acquired_at,l.refreshed_at,l.expires_at,u.display_name FROM file_locks l JOIN files f ON f.id=l.file_id AND f.org_id=l.org_id JOIN users u ON u.id=l.holder_user_id WHERE l.org_id=$1 AND f.project_id=$2 AND f.app <> 'figcraft' AND l.expires_at>CURRENT_TIMESTAMP AND l.holder_session_id<>$3 ORDER BY l.file_id LIMIT 1",
        ).bind(self.org_id).bind(project_id).bind(current_session_id).fetch_optional(&mut *self.tx).await?)
    }

    async fn session_is_active(
        &mut self,
        session: LockSessionContext,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        if now >= session.expires_at {
            return Ok(false);
        }
        Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id=$1 AND s.user_id=$2 AND s.revoked_at IS NULL AND s.absolute_expires_at>$3 AND s.last_seen_at + ($4::bigint * interval '1 second') > $3 AND u.disabled_at IS NULL)")
            .bind(session.session_id).bind(session.user_id).bind(now).bind(session.idle_timeout_secs).fetch_one(&mut *self.tx).await?)
    }
    pub async fn lock_acquire(
        &mut self,
        file_id: Uuid,
        session: LockSessionContext,
        client_id: &str,
        takeover: bool,
    ) -> Result<AcquireOutcome, DbError> {
        let now: OffsetDateTime = sqlx::query_scalar("SELECT CURRENT_TIMESTAMP")
            .fetch_one(&mut *self.tx)
            .await?;
        if !self.session_is_active(session, now).await? {
            return Ok(AcquireOutcome::SessionExpired);
        }
        sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND file_id=$2 AND expires_at<=$3")
            .bind(self.org_id)
            .bind(file_id)
            .bind(now)
            .execute(&mut *self.tx)
            .await?;
        let current = sqlx::query_as::<_, FileLock>(
            "SELECT l.file_id,l.org_id,l.holder_user_id,l.holder_session_kind,l.holder_session_id,l.client_id,l.acquired_at,l.refreshed_at,l.expires_at,u.display_name FROM file_locks l JOIN users u ON u.id=l.holder_user_id WHERE l.org_id=$1 AND l.file_id=$2 FOR UPDATE OF l",
        ).bind(self.org_id).bind(file_id).fetch_optional(&mut *self.tx).await?;
        if let Some(current) = current {
            let same_session = current.holder_user_id == session.user_id
                && current.holder_session_id == session.session_id;
            let same_user = current.holder_user_id == session.user_id;
            if !same_session && !(same_user && takeover) {
                return Ok(AcquireOutcome::Locked { current, same_user });
            }
            let updated = sqlx::query_as::<_, FileLock>(
                "UPDATE file_locks l SET holder_user_id=$3,holder_session_kind=$4,holder_session_id=$5,client_id=$6,acquired_at=CASE WHEN $7 THEN $8 ELSE l.acquired_at END,refreshed_at=$8,expires_at=$9 FROM users u WHERE l.file_id=$1 AND l.org_id=$2 AND u.id=$3 RETURNING l.file_id,l.org_id,l.holder_user_id,l.holder_session_kind,l.holder_session_id,l.client_id,l.acquired_at,l.refreshed_at,l.expires_at,u.display_name",
            ).bind(file_id).bind(self.org_id).bind(session.user_id).bind(session.kind.as_str()).bind(session.session_id).bind(client_id).bind(!same_session).bind(now).bind(now+Duration::seconds(LEASE_SECONDS)).fetch_one(&mut *self.tx).await?;
            return Ok(AcquireOutcome::Acquired {
                current: updated,
                replaced_session: !same_session,
                created: false,
            });
        }
        let inserted = sqlx::query_as::<_, FileLock>(
            "INSERT INTO file_locks (file_id,org_id,holder_user_id,holder_session_kind,holder_session_id,client_id,acquired_at,refreshed_at,expires_at) SELECT $1,$2,$3,$4,$5,$6,$7,$7,$8 FROM users u WHERE u.id=$3 AND u.disabled_at IS NULL RETURNING file_id,org_id,holder_user_id,holder_session_kind,holder_session_id,client_id,acquired_at,refreshed_at,expires_at,(SELECT display_name FROM users WHERE id=$3) AS display_name",
        ).bind(file_id).bind(self.org_id).bind(session.user_id).bind(session.kind.as_str()).bind(session.session_id).bind(client_id).bind(now).bind(now+Duration::seconds(LEASE_SECONDS)).fetch_optional(&mut *self.tx).await?;
        inserted
            .map(|current| AcquireOutcome::Acquired {
                current,
                replaced_session: false,
                created: true,
            })
            .ok_or(DbError::Internal)
    }

    pub async fn lock_release(
        &mut self,
        file_id: Uuid,
        session: LockSessionContext,
        force: bool,
    ) -> Result<bool, DbError> {
        let now: OffsetDateTime = sqlx::query_scalar("SELECT CURRENT_TIMESTAMP")
            .fetch_one(&mut *self.tx)
            .await?;
        if !self.session_is_active(session, now).await? {
            return Ok(false);
        }
        let result = if force {
            sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND file_id=$2")
                .bind(self.org_id)
                .bind(file_id)
                .execute(&mut *self.tx)
                .await?
        } else {
            sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND file_id=$2 AND holder_user_id=$3 AND holder_session_id=$4")
                .bind(self.org_id).bind(file_id).bind(session.user_id).bind(session.session_id).execute(&mut *self.tx).await?
        };
        Ok(result.rows_affected() == 1)
    }
}

pub async fn count_expired(pool: &PgPool) -> Result<i64, DbError> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM file_locks WHERE expires_at <= CURRENT_TIMESTAMP")
            .fetch_one(pool)
            .await?,
    )
}
pub async fn orgs_with_expired(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT org_id FROM file_locks WHERE expires_at <= CURRENT_TIMESTAMP ORDER BY org_id LIMIT $1",
    )
    .bind(limit.clamp(1, 1000))
    .fetch_all(pool)
    .await?)
}
pub async fn expire_for_org(pool: &PgPool, org_id: Uuid, limit: i64) -> Result<u64, DbError> {
    let Some(mut tx) = OrgTx::begin(pool, org_id).await? else {
        return Ok(0);
    };
    let deleted = sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND file_id IN (SELECT file_id FROM file_locks WHERE org_id=$1 AND expires_at <= CURRENT_TIMESTAMP ORDER BY expires_at,file_id LIMIT $2 FOR UPDATE SKIP LOCKED)")
        .bind(org_id).bind(limit.clamp(1, 1000)).execute(&mut *tx.tx).await?.rows_affected();
    tx.commit().await?;
    Ok(deleted)
}
