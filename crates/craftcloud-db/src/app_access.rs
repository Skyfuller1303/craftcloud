//! Organization app assignment repository.

use crate::audit::{AuditCtx, Detail, NewEvent, record_in};
use crate::{DbError, OrgScope, members::OrgTx};
use craftcloud_core::policy::Action;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AppUsage {
    pub app: String,
    pub assigned_members: i64,
    pub active_devices: i64,
    pub storage_bytes: i64,
}

pub async fn seat_usage(pool: &PgPool, scope: &OrgScope) -> Result<(i64, Option<i32>), DbError> {
    scope.require(Action::OrganizationOverviewRead)?;
    Ok(sqlx::query_as("SELECT (SELECT count(*) FROM organization_members m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.status='active' AND m.role <> 'guest' AND u.disabled_at IS NULL), o.seat_limit FROM organizations o WHERE o.id=$1")
        .bind(scope.org_id()).fetch_one(pool).await?)
}

pub async fn usage(
    pool: &PgPool,
    scope: &OrgScope,
    app_ids: &[String],
) -> Result<(String, Option<i32>, i64, Vec<AppUsage>), DbError> {
    scope.require(Action::OrganizationAppsRead)?;
    let (mode, limit, used): (String, Option<i32>, i64) = sqlx::query_as(
        "SELECT o.app_access_mode,o.seat_limit,(SELECT count(*) FROM organization_members m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.status='active' AND m.role <> 'guest' AND u.disabled_at IS NULL) FROM organizations o WHERE o.id=$1",
    ).bind(scope.org_id()).fetch_one(pool).await?;
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT a.app, \
           (SELECT count(DISTINCT ma.user_id) FROM member_app_access ma \
             JOIN organization_members m ON m.org_id=ma.org_id AND m.user_id=ma.user_id \
             WHERE ma.org_id=$1 AND ma.app=a.app AND m.status='active' \
               AND m.role NOT IN ('owner','admin','guest')), \
           count(DISTINCT d.id), \
           COALESCE((SELECT sum(f.size_bytes)::bigint FROM files f WHERE f.org_id=$1 AND f.app=a.app AND f.deleted_at IS NULL),0::bigint) \
         FROM (SELECT unnest($2::text[]) app) a \
         LEFT JOIN device_sessions d ON d.client_kind='app' AND d.client_id=a.app \
           AND d.revoked_at IS NULL AND d.idle_expires_at>now() AND d.absolute_expires_at>now() \
           AND EXISTS(SELECT 1 FROM organization_members m \
             JOIN organizations o ON o.id=m.org_id JOIN users u ON u.id=m.user_id \
             WHERE m.org_id=$1 AND m.user_id=d.user_id AND m.status='active' AND u.disabled_at IS NULL \
               AND (o.app_access_mode='all' OR m.role IN ('owner','admin','guest') OR \
                 EXISTS(SELECT 1 FROM member_app_access ma WHERE ma.org_id=m.org_id AND ma.user_id=m.user_id AND ma.app=a.app))) \
         GROUP BY a.app ORDER BY a.app",
    ).bind(scope.org_id()).bind(app_ids).fetch_all(pool).await?;
    Ok((
        mode,
        limit,
        used,
        rows.into_iter()
            .map(
                |(app, assigned_members, active_devices, storage_bytes)| AppUsage {
                    app,
                    assigned_members,
                    active_devices,
                    storage_bytes,
                },
            )
            .collect(),
    ))
}

pub async fn assigned(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    app: &str,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND m.role NOT IN ('owner','admin','guest') AND o.app_access_mode='assigned' AND EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=m.org_id AND a.user_id=m.user_id AND a.app=$3))")
        .bind(org_id).bind(user_id).bind(app).fetch_one(pool).await?)
}

pub async fn is_allowed(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    app: &str,
    app_ids: &[String],
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND (o.app_access_mode='all' OR (($3 = ANY($4::text[])) AND (m.role IN ('owner','admin','guest') OR EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=m.org_id AND a.user_id=m.user_id AND a.app=$3)))))")
        .bind(org_id).bind(user_id).bind(app).bind(app_ids).fetch_one(pool).await?)
}

pub async fn is_allowed_tx(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    user_id: Uuid,
    app: &str,
    app_ids: &[String],
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND (o.app_access_mode='all' OR (($3 = ANY($4::text[])) AND (m.role IN ('owner','admin','guest') OR EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=m.org_id AND a.user_id=m.user_id AND a.app=$3)))))")
        .bind(org_id).bind(user_id).bind(app).bind(app_ids).fetch_one(&mut **tx).await?)
}

/// Check device login after 2FA and serialize against organization app-policy
/// changes. Multi-organization accounts need access in at least one active
/// organization; each request is still checked against its own tenant.
pub async fn device_login_allowed(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    kind: &str,
    app: &str,
    app_ids: &[String],
) -> Result<bool, DbError> {
    if kind != "app" {
        return Ok(true);
    }
    let _: Vec<Uuid> = sqlx::query_scalar("SELECT o.id FROM organizations o JOIN organization_members m ON m.org_id=o.id WHERE m.user_id=$1 AND m.status='active' ORDER BY o.id FOR SHARE OF o")
        .bind(user_id).fetch_all(&mut **tx).await?;
    Ok(sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM organization_members m WHERE m.user_id=$1 AND m.status='active') OR EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.user_id=$1 AND m.status='active' AND (o.app_access_mode='all' OR (($2 = ANY($3::text[])) AND (m.role IN ('owner','admin','guest') OR EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=m.org_id AND a.user_id=m.user_id AND a.app=$2)))))")
        .bind(user_id).bind(app).bind(app_ids).fetch_one(&mut **tx).await?)
}

pub async fn set_assignments(
    pool: &PgPool,
    scope: &OrgScope,
    user_id: Uuid,
    apps: &[String],
    audit: &AuditCtx,
) -> Result<(), DbError> {
    scope.require(Action::MemberAppsSet)?;
    let Some(mut org_tx) = OrgTx::begin(pool, scope.org_id()).await? else {
        return Err(DbError::Internal);
    };
    set_assignments_tx(
        &mut org_tx,
        scope,
        user_id,
        apps,
        time::OffsetDateTime::now_utc(),
        audit,
    )
    .await?;
    org_tx.commit().await?;
    Ok(())
}

pub async fn set_assignments_tx(
    org_tx: &mut OrgTx,
    scope: &OrgScope,
    user_id: Uuid,
    apps: &[String],
    now: time::OffsetDateTime,
    audit: &AuditCtx,
) -> Result<(), DbError> {
    scope.require(Action::MemberAppsSet)?;
    if scope.org_id() != org_tx.org_id {
        return Err(DbError::Internal);
    }
    let tx = &mut org_tx.tx;
    let before: Vec<String> = sqlx::query_scalar(
        "SELECT app FROM member_app_access WHERE org_id=$1 AND user_id=$2 ORDER BY app",
    )
    .bind(scope.org_id())
    .bind(user_id)
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM member_app_access WHERE org_id=$1 AND user_id=$2")
        .bind(scope.org_id())
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    for app in apps {
        sqlx::query("INSERT INTO member_app_access(org_id,user_id,app) SELECT $1,$2,$3 WHERE EXISTS(SELECT 1 FROM organization_members WHERE org_id=$1 AND user_id=$2 AND status='active' AND role <> 'guest')")
            .bind(scope.org_id()).bind(user_id).bind(app).execute(&mut **tx).await?;
    }
    for app in before.iter().filter(|app| !apps.contains(app)) {
        crate::figcraft::enqueue_user_app_revocations(tx, scope.org_id(), user_id, app, now)
            .await?;
        revoke_app_sessions(tx, scope.org_id(), user_id, app, audit).await?;
    }
    record_in(
        tx,
        audit,
        &NewEvent::new("member.apps_changed")
            .org(scope.org_id())
            .target("user", user_id)
            .detail("count", Detail::Count(apps.len() as i64)),
    )
    .await?;
    Ok(())
}

pub async fn set_mode(
    pool: &PgPool,
    scope: &OrgScope,
    mode: &str,
    audit: &AuditCtx,
) -> Result<(), DbError> {
    scope.require(Action::OrganizationAppModeSet)?;
    let Some(mut org_tx) = OrgTx::begin(pool, scope.org_id()).await? else {
        return Err(DbError::Internal);
    };
    set_mode_tx(
        &mut org_tx,
        scope,
        mode,
        time::OffsetDateTime::now_utc(),
        audit,
    )
    .await?;
    org_tx.commit().await?;
    Ok(())
}

pub async fn set_mode_tx(
    org_tx: &mut OrgTx,
    scope: &OrgScope,
    mode: &str,
    now: time::OffsetDateTime,
    audit: &AuditCtx,
) -> Result<(), DbError> {
    scope.require(Action::OrganizationAppModeSet)?;
    if scope.org_id() != org_tx.org_id {
        return Err(DbError::Internal);
    }
    let tx = &mut org_tx.tx;
    let previous: String =
        sqlx::query_scalar("SELECT app_access_mode FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(scope.org_id())
            .fetch_one(&mut **tx)
            .await?;
    sqlx::query("UPDATE organizations SET app_access_mode=$2 WHERE id=$1")
        .bind(scope.org_id())
        .bind(mode)
        .execute(&mut **tx)
        .await?;
    if previous == "all" && mode == "assigned" {
        crate::figcraft::enqueue_org_app_revocations(tx, scope.org_id(), now).await?;
        let sessions:Vec<(Uuid,Uuid,String)>=sqlx::query_as("SELECT d.id,d.user_id,d.client_id FROM device_sessions d JOIN organization_members m ON m.org_id=$1 AND m.user_id=d.user_id AND m.status='active' AND m.role NOT IN ('owner','admin','guest') WHERE d.client_kind='app' AND d.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=$1 AND a.user_id=d.user_id AND a.app=d.client_id)").bind(scope.org_id()).fetch_all(&mut **tx).await?;
        for (session, user, app) in sessions {
            sqlx::query("UPDATE device_sessions SET revoked_at=now(),revoke_reason='security',access_hash=NULL,access_expires_at=NULL WHERE id=$1 AND revoked_at IS NULL").bind(session).execute(&mut **tx).await?;
            sqlx::query("DELETE FROM file_locks WHERE holder_session_kind='device' AND holder_session_id=$1").bind(session).execute(&mut **tx).await?;
            record_in(
                tx,
                audit,
                &NewEvent::new("auth.device_revoked")
                    .org(scope.org_id())
                    .target("device_session", session)
                    .detail("reason", Detail::Code("security")),
            )
            .await?;
            let _ = user;
            let _ = app;
        }
    }
    record_in(
        tx,
        audit,
        &NewEvent::new("organization.app_mode_changed")
            .org(scope.org_id())
            .detail(
                "mode",
                Detail::Code(if mode == "assigned" {
                    "assigned"
                } else {
                    "all"
                }),
            ),
    )
    .await?;
    Ok(())
}

async fn revoke_app_sessions(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    org: Uuid,
    user: Uuid,
    app: &str,
    audit: &AuditCtx,
) -> Result<(), DbError> {
    let ids:Vec<Uuid>=sqlx::query_scalar("UPDATE device_sessions SET revoked_at=now(),revoke_reason='security',access_hash=NULL,access_expires_at=NULL WHERE user_id=$1 AND client_kind='app' AND client_id=$2 AND revoked_at IS NULL RETURNING id").bind(user).bind(app).fetch_all(&mut **tx).await?;
    for id in ids {
        sqlx::query(
            "DELETE FROM file_locks WHERE holder_session_kind='device' AND holder_session_id=$1",
        )
        .bind(id)
        .execute(&mut **tx)
        .await?;
        record_in(
            tx,
            audit,
            &NewEvent::new("auth.device_revoked")
                .org(org)
                .target("device_session", id)
                .detail("reason", Detail::Code("security")),
        )
        .await?;
    }
    Ok(())
}

pub async fn set_seat_limit(
    pool: &PgPool,
    org_id: Uuid,
    limit: Option<i32>,
    audit: &AuditCtx,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let found = sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org_id)
        .fetch_optional(&mut *tx)
        .await?;
    if found.is_none() {
        return Ok(false);
    }
    sqlx::query("UPDATE organizations SET seat_limit=$2 WHERE id=$1")
        .bind(org_id)
        .bind(limit)
        .execute(&mut *tx)
        .await?;
    let detail = limit.map_or(Detail::Code("unlimited"), |value| {
        Detail::Count(i64::from(value))
    });
    record_in(
        &mut tx,
        audit,
        &NewEvent::new("organization.seat_limit_changed")
            .org(org_id)
            .target("organization", org_id)
            .detail("seat_limit", detail),
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}
