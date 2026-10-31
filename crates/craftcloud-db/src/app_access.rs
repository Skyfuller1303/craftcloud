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
    // Drive from the organization's members so device sessions are reached
    // through `device_sessions_user_idx`, and aggregate files once per app.
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "WITH mem AS (SELECT m.user_id, m.role FROM organization_members m              JOIN users u ON u.id=m.user_id              WHERE m.org_id=$1 AND m.status='active' AND u.disabled_at IS NULL),          asgn AS (SELECT ma.app, count(DISTINCT ma.user_id) AS n FROM member_app_access ma              JOIN organization_members m ON m.org_id=ma.org_id AND m.user_id=ma.user_id              WHERE ma.org_id=$1 AND m.status='active' AND m.role NOT IN ('owner','admin','guest')              GROUP BY ma.app),          dev AS (SELECT d.client_id AS app, count(DISTINCT d.id) AS n FROM mem              JOIN device_sessions d ON d.user_id=mem.user_id AND d.client_kind='app'                AND d.client_id = ANY($2::text[]) AND d.revoked_at IS NULL                AND d.idle_expires_at>now() AND d.absolute_expires_at>now()              WHERE $3='all' OR mem.role IN ('owner','admin','guest') OR EXISTS(SELECT 1                FROM member_app_access ma WHERE ma.org_id=$1 AND ma.user_id=mem.user_id AND ma.app=d.client_id)              GROUP BY d.client_id),          stor AS (SELECT f.app, COALESCE(sum(v.size_bytes),0)::bigint AS n FROM files f              JOIN file_versions v ON v.org_id=f.org_id AND v.file_id=f.id              WHERE f.org_id=$1 AND f.app = ANY($2::text[]) GROUP BY f.app)          SELECT a.app, COALESCE(asgn.n,0)::bigint, COALESCE(dev.n,0)::bigint, COALESCE(stor.n,0)::bigint          FROM (SELECT DISTINCT unnest($2::text[]) AS app) a          LEFT JOIN asgn ON asgn.app=a.app LEFT JOIN dev ON dev.app=a.app LEFT JOIN stor ON stor.app=a.app          ORDER BY a.app",
    ).bind(scope.org_id()).bind(app_ids).bind(&mode).fetch_all(pool).await?;
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

/// True when at least one active membership of `$1` allows app `$2` (with
/// catalog `$3`). Shared by device login and by session revocation so that
/// both agree on what "the account may still use this app" means.
const ANY_ORG_ALLOWS_APP: &str = "EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.user_id=$1 AND m.status='active' AND (o.app_access_mode='all' OR (($2 = ANY($3::text[])) AND (m.role IN ('owner','admin','guest') OR EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=m.org_id AND a.user_id=m.user_id AND a.app=$2)))))";

async fn any_org_allows_app(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    app: &str,
    app_ids: &[String],
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(&format!("SELECT {ANY_ORG_ALLOWS_APP}"))
        .bind(user_id)
        .bind(app)
        .bind(app_ids)
        .fetch_one(&mut **tx)
        .await?)
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
    Ok(sqlx::query_scalar(&format!("SELECT NOT EXISTS(SELECT 1 FROM organization_members m WHERE m.user_id=$1 AND m.status='active') OR {ANY_ORG_ALLOWS_APP}"))
        .bind(user_id).bind(app).bind(app_ids).fetch_one(&mut **tx).await?)
}

/// Whether launcher and CLI device sessions of this user may perform the
/// editing actions that are reserved for assigned apps (file locks, FigCraft
/// live links) in this organization. They are not catalog apps, so in
/// `assigned` mode only owners, admins and guests keep them, like their
/// "always all apps" rule; everybody else is limited to browser-equivalent
/// actions.
pub async fn tool_allowed(pool: &PgPool, org_id: Uuid, user_id: Uuid) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND (o.app_access_mode='all' OR m.role IN ('owner','admin','guest')))")
        .bind(org_id).bind(user_id).fetch_one(pool).await?)
}

pub async fn set_assignments(
    pool: &PgPool,
    scope: &OrgScope,
    user_id: Uuid,
    apps: &[String],
    app_ids: &[String],
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
        app_ids,
        time::OffsetDateTime::now_utc(),
        audit,
    )
    .await?;
    org_tx.commit().await?;
    Ok(())
}

/// Replaces the assignments of one member. Assignments only exist for the
/// role `member`; owners and admins always have every app and guests have no
/// assignment, so rows are written for members only.
pub async fn set_assignments_tx(
    org_tx: &mut OrgTx,
    scope: &OrgScope,
    user_id: Uuid,
    apps: &[String],
    app_ids: &[String],
    now: time::OffsetDateTime,
    audit: &AuditCtx,
) -> Result<(), DbError> {
    scope.require(Action::MemberAppsSet)?;
    if scope.org_id() != org_tx.org_id {
        return Err(DbError::Internal);
    }
    let tx = &mut org_tx.tx;
    lock_member(tx, scope.org_id(), user_id).await?;
    let before = current_assignments(tx, scope.org_id(), user_id).await?;
    write_assignments(
        tx,
        scope.org_id(),
        user_id,
        &before,
        apps,
        app_ids,
        now,
        audit,
    )
    .await
}

/// Adds and removes single apps relative to the stored assignments, so
/// concurrent changes to other apps are not overwritten. Returns the
/// resulting assignments.
#[allow(clippy::too_many_arguments)]
pub async fn change_assignments_tx(
    org_tx: &mut OrgTx,
    scope: &OrgScope,
    user_id: Uuid,
    add: &[String],
    remove: &[String],
    app_ids: &[String],
    now: time::OffsetDateTime,
    audit: &AuditCtx,
) -> Result<Vec<String>, DbError> {
    scope.require(Action::MemberAppsSet)?;
    if scope.org_id() != org_tx.org_id {
        return Err(DbError::Internal);
    }
    let tx = &mut org_tx.tx;
    lock_member(tx, scope.org_id(), user_id).await?;
    let before = current_assignments(tx, scope.org_id(), user_id).await?;
    let mut after: Vec<String> = before
        .iter()
        .filter(|app| !remove.contains(app))
        .cloned()
        .collect();
    for app in add {
        if !after.contains(app) {
            after.push(app.clone());
        }
    }
    after.sort();
    write_assignments(
        tx,
        scope.org_id(),
        user_id,
        &before,
        &after,
        app_ids,
        now,
        audit,
    )
    .await?;
    current_assignments(tx, scope.org_id(), user_id).await
}

/// Serializes concurrent assignment changes of one member.
async fn lock_member(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    user: Uuid,
) -> Result<(), DbError> {
    sqlx::query("SELECT 1 FROM organization_members WHERE org_id=$1 AND user_id=$2 FOR UPDATE")
        .bind(org)
        .bind(user)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(())
}

async fn current_assignments(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    user: Uuid,
) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT app FROM member_app_access WHERE org_id=$1 AND user_id=$2 ORDER BY app",
    )
    .bind(org)
    .bind(user)
    .fetch_all(&mut **tx)
    .await?)
}

/// Revocation only applies where an assignment is actually enforced: an
/// `assigned` organization and a plain `member`. Removing a row for an
/// owner, admin or guest, or in an `all` organization, changes no access.
async fn enforcement_applies(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    user: Uuid,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND m.role='member' AND o.app_access_mode='assigned')")
        .bind(org).bind(user).fetch_one(&mut **tx).await?)
}

#[allow(clippy::too_many_arguments)]
async fn write_assignments(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    user_id: Uuid,
    before: &[String],
    apps: &[String],
    app_ids: &[String],
    now: time::OffsetDateTime,
    audit: &AuditCtx,
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM member_app_access WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    for app in apps {
        sqlx::query("INSERT INTO member_app_access(org_id,user_id,app) SELECT $1,$2,$3 WHERE EXISTS(SELECT 1 FROM organization_members WHERE org_id=$1 AND user_id=$2 AND status='active' AND role='member')")
            .bind(org).bind(user_id).bind(app).execute(&mut **tx).await?;
    }
    if enforcement_applies(tx, org, user_id).await? {
        for app in before.iter().filter(|app| !apps.contains(app)) {
            crate::figcraft::enqueue_user_app_revocations(tx, org, user_id, app, now).await?;
            revoke_app_sessions(tx, org, user_id, app, app_ids, audit).await?;
        }
    }
    record_in(
        tx,
        audit,
        &NewEvent::new("member.apps_changed")
            .org(org)
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
    app_ids: &[String],
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
        app_ids,
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
    app_ids: &[String],
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
        let sessions:Vec<(Uuid,Uuid,String)>=sqlx::query_as("SELECT d.id,d.user_id,d.client_id FROM device_sessions d JOIN organization_members m ON m.org_id=$1 AND m.user_id=d.user_id AND m.status='active' AND m.role='member' WHERE d.client_kind='app' AND d.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=$1 AND a.user_id=d.user_id AND a.app=d.client_id) ORDER BY d.id").bind(scope.org_id()).fetch_all(&mut **tx).await?;
        for (session, user, app) in sessions {
            release_app_session(tx, scope.org_id(), session, user, &app, app_ids, audit).await?;
        }
        // Launcher and CLI sessions of plain members lose the editing actions
        // reserved for assigned apps; the sessions stay usable like a browser.
        sqlx::query("DELETE FROM file_locks l WHERE l.org_id=$1 AND l.holder_session_kind='device' AND EXISTS(SELECT 1 FROM device_sessions d WHERE d.id=l.holder_session_id AND d.client_kind IN ('launcher','cli')) AND EXISTS(SELECT 1 FROM organization_members m WHERE m.org_id=l.org_id AND m.user_id=l.holder_user_id AND m.status='active' AND m.role='member')")
            .bind(scope.org_id()).execute(&mut **tx).await?;
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

/// Device sessions have no organization column. When this organization stops
/// allowing `app` for `user`, a session is therefore ended only if no other
/// active membership of the user still allows the app; otherwise only the
/// locks on this organization's files are released for the session.
async fn release_app_session(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    session: Uuid,
    user: Uuid,
    app: &str,
    app_ids: &[String],
    audit: &AuditCtx,
) -> Result<(), DbError> {
    if any_org_allows_app(tx, user, app, app_ids).await? {
        sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND holder_session_kind='device' AND holder_session_id=$2")
            .bind(org).bind(session).execute(&mut **tx).await?;
        return Ok(());
    }
    let ended = sqlx::query("UPDATE device_sessions SET revoked_at=now(),revoke_reason='security',access_hash=NULL,access_expires_at=NULL WHERE id=$1 AND revoked_at IS NULL")
        .bind(session).execute(&mut **tx).await?.rows_affected();
    sqlx::query(
        "DELETE FROM file_locks WHERE holder_session_kind='device' AND holder_session_id=$1",
    )
    .bind(session)
    .execute(&mut **tx)
    .await?;
    if ended > 0 {
        record_in(
            tx,
            audit,
            &NewEvent::new("auth.device_revoked")
                .org(org)
                .target("device_session", session)
                .detail("reason", Detail::Code("security")),
        )
        .await?;
    }
    Ok(())
}

async fn revoke_app_sessions(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    user: Uuid,
    app: &str,
    app_ids: &[String],
    audit: &AuditCtx,
) -> Result<(), DbError> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM device_sessions WHERE user_id=$1 AND client_kind='app' AND client_id=$2 AND revoked_at IS NULL ORDER BY id FOR UPDATE")
        .bind(user).bind(app).fetch_all(&mut **tx).await?;
    for id in ids {
        release_app_session(tx, org, id, user, app, app_ids, audit).await?;
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
