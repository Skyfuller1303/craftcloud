//! Tenant-scoped FigCraft live metadata and durable remote-operation queue.

use sqlx::{FromRow, PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::members::OrgTx;
use crate::{DbError, OrgScope};

#[derive(Debug, Clone, FromRow)]
pub struct LiveFile {
    pub file_id: Uuid,
    pub org_id: Uuid,
    pub workspace_id: Uuid,
    pub figcraft_file_id: Option<String>,
    pub published_from_version_id: Uuid,
    pub last_snapshot_version_id: Option<Uuid>,
    pub last_snapshot_at: Option<OffsetDateTime>,
    pub state: String,
    pub published_by_user_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishIntentOutcome {
    Started,
    HeadChanged,
    Busy,
    Gone,
}

#[derive(Debug, Clone, FromRow)]
pub struct LiveLink {
    pub id: Uuid,
    pub file_id: Uuid,
    pub user_id: Uuid,
    pub access: String,
    pub figcraft_link_id: Option<String>,
    pub token_ciphertext: Vec<u8>,
    pub created_at: OffsetDateTime,
    pub revoked_at: Option<OffsetDateTime>,
}

pub async fn get(
    pool: &PgPool,
    scope: &OrgScope,
    file_id: Uuid,
) -> Result<Option<LiveFile>, DbError> {
    Ok(sqlx::query_as("SELECT l.file_id,l.org_id,l.workspace_id,l.figcraft_file_id,l.published_from_version_id,l.last_snapshot_version_id,l.last_snapshot_at,l.state,l.published_by_user_id FROM figcraft_live_files l JOIN files f ON f.id=l.file_id AND f.org_id=l.org_id WHERE l.org_id=$1 AND l.file_id=$2 AND f.deleted_at IS NULL")
        .bind(scope.org_id()).bind(file_id).fetch_optional(pool).await?)
}

pub async fn get_any(pool: &PgPool, file_id: Uuid) -> Result<Option<LiveFile>, DbError> {
    Ok(sqlx::query_as("SELECT file_id,org_id,workspace_id,figcraft_file_id,published_from_version_id,last_snapshot_version_id,last_snapshot_at,state,published_by_user_id FROM figcraft_live_files WHERE file_id=$1")
        .bind(file_id).fetch_optional(pool).await?)
}

pub async fn live_link(
    pool: &PgPool,
    file_id: Uuid,
    user_id: Uuid,
    access: &str,
    source_kind: &str,
    source_app: Option<&str>,
) -> Result<Option<LiveLink>, DbError> {
    Ok(sqlx::query_as("SELECT id,file_id,user_id,access,figcraft_link_id,token_ciphertext,created_at,revoked_at FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access=$3 AND source_kind=$4 AND source_app IS NOT DISTINCT FROM $5 AND revoked_at IS NULL ORDER BY created_at DESC LIMIT 1")
        .bind(file_id).bind(user_id).bind(access).bind(source_kind).bind(source_app).fetch_optional(pool).await?)
}

pub async fn links_for_source(
    pool: &PgPool,
    file_id: Uuid,
    user_id: Uuid,
    access: &str,
    source_kind: &str,
    source_app: Option<&str>,
) -> Result<Vec<LiveLink>, DbError> {
    Ok(sqlx::query_as("SELECT id,file_id,user_id,access,figcraft_link_id,token_ciphertext,created_at,revoked_at FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access=$3 AND source_kind=$4 AND source_app IS NOT DISTINCT FROM $5 AND revoked_at IS NULL ORDER BY id")
        .bind(file_id).bind(user_id).bind(access).bind(source_kind).bind(source_app).fetch_all(pool).await?)
}

pub async fn links(pool: &PgPool, file_id: Uuid) -> Result<Vec<LiveLink>, DbError> {
    Ok(sqlx::query_as("SELECT id,file_id,user_id,access,figcraft_link_id,token_ciphertext,created_at,revoked_at FROM figcraft_links WHERE file_id=$1 AND revoked_at IS NULL ORDER BY id")
        .bind(file_id).fetch_all(pool).await?)
}

pub async fn insert_live(
    pool: &PgPool,
    live: &LiveFile,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO figcraft_live_files(file_id,org_id,workspace_id,published_from_version_id,state,published_by_user_id,created_at,updated_at) VALUES($1,$2,$3,$4,'publishing',$5,$6,$6) ON CONFLICT(file_id) DO UPDATE SET org_id=EXCLUDED.org_id,workspace_id=EXCLUDED.workspace_id,published_from_version_id=EXCLUDED.published_from_version_id,published_by_user_id=EXCLUDED.published_by_user_id,state='publishing',figcraft_file_id=NULL,final_snapshot_done=false,updated_at=EXCLUDED.updated_at WHERE figcraft_live_files.state='republish'")
        .bind(live.file_id).bind(live.org_id).bind(live.workspace_id).bind(live.published_from_version_id).bind(live.published_by_user_id).bind(now).execute(pool).await?;
    Ok(())
}

pub async fn mark_published(
    pool: &PgPool,
    file_id: Uuid,
    remote_id: &str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_live_files SET figcraft_file_id=$2,state='live',updated_at=$3 WHERE file_id=$1 AND state='publishing'")
        .bind(file_id).bind(remote_id).bind(now).execute(pool).await?;
    Ok(())
}

pub async fn mark_published_audited(
    pool: &PgPool,
    file_id: Uuid,
    remote_id: &str,
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let row:Option<(Uuid,Uuid)>=sqlx::query_as("UPDATE figcraft_live_files l SET figcraft_file_id=$2,state='live',updated_at=$3 WHERE l.file_id=$1 AND l.state='publishing' AND EXISTS(SELECT 1 FROM files f JOIN projects p ON p.id=f.project_id WHERE f.id=l.file_id AND f.head_version_id=l.published_from_version_id AND f.deleted_at IS NULL AND p.deleted_at IS NULL) RETURNING l.org_id,l.workspace_id").bind(file_id).bind(remote_id).bind(now).fetch_optional(&mut *tx).await?;
    let Some((org_id, workspace_id)) = row else {
        return Err(DbError::Internal);
    };
    let event = crate::audit::NewEvent::new("figcraft.published")
        .org(org_id)
        .target("file", file_id)
        .workspace(workspace_id);
    crate::audit::record_in(&mut tx, ctx, &event).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn clear_publish_intent(
    pool: &PgPool,
    file_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_live_files SET state='republish',updated_at=$2 WHERE file_id=$1 AND state='publishing' AND figcraft_file_id IS NULL")
        .bind(file_id)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_unpublishing(
    pool: &PgPool,
    file_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE figcraft_live_files SET state='unpublishing',final_snapshot_done=false,last_snapshot_attempt_at=NULL,updated_at=$2 WHERE file_id=$1 AND state='live'").bind(file_id).bind(now).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,l.user_id,'unpublish',$2,$2 FROM figcraft_links l WHERE l.file_id=$1 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)").bind(file_id).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn finish_unpublish(
    pool: &PgPool,
    file_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_live_files SET state='republish',figcraft_file_id=NULL,updated_at=$2 WHERE file_id=$1 AND state='unpublishing'").bind(file_id).bind(now).execute(pool).await?;
    Ok(())
}

pub async fn finish_unpublish_audited(
    pool: &PgPool,
    file_id: Uuid,
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let row:Option<(Uuid,Uuid)>=sqlx::query_as("UPDATE figcraft_live_files SET state='republish',figcraft_file_id=NULL,updated_at=$2 WHERE file_id=$1 AND state='unpublishing' RETURNING org_id,workspace_id").bind(file_id).bind(now).fetch_optional(&mut *tx).await?;
    if let Some((org_id, workspace_id)) = row {
        let event = crate::audit::NewEvent::new("figcraft.unpublished")
            .org(org_id)
            .target("file", file_id)
            .workspace(workspace_id);
        crate::audit::record_in(&mut tx, ctx, &event).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub struct NewFigcraftLink<'a> {
    pub id: Uuid,
    pub file_id: Uuid,
    pub user_id: Uuid,
    pub access: &'a str,
    pub remote_file: &'a str,
    pub remote_link: &'a str,
    pub ciphertext: &'a [u8],
    pub now: OffsetDateTime,
    pub workspace_id: Uuid,
    pub source_kind: &'a str,
    pub source_app: Option<&'a str>,
}

pub async fn revoke_link_audited(
    pool: &PgPool,
    id: Uuid,
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let row:Option<(Uuid,Uuid,String)>=sqlx::query_as("UPDATE figcraft_links SET revoked_at=$2 WHERE id=$1 AND revoked_at IS NULL RETURNING file_id,user_id,access").bind(id).bind(now).fetch_optional(&mut *tx).await?;
    if let Some((file_id, user_id, access)) = row {
        let (org_id, workspace_id): (Uuid, Uuid) =
            sqlx::query_as("SELECT org_id,workspace_id FROM figcraft_live_files WHERE file_id=$1")
                .bind(file_id)
                .fetch_one(&mut *tx)
                .await?;
        let mut event = crate::audit::NewEvent::new("figcraft.link_revoked")
            .org(org_id)
            .target("file", file_id)
            .workspace(workspace_id);
        event
            .details
            .push(("link_id", crate::audit::Detail::Id(id)));
        event
            .details
            .push(("user_id", crate::audit::Detail::Id(user_id)));
        event.details.push((
            "access",
            crate::audit::Detail::Code(if access == "edit" { "edit" } else { "view" }),
        ));
        crate::audit::record_in(&mut tx, ctx, &event).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn update_link_ciphertext(
    pool: &PgPool,
    id: Uuid,
    ciphertext: &[u8],
) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_links SET token_ciphertext=$2 WHERE id=$1 AND revoked_at IS NULL")
        .bind(id)
        .bind(ciphertext)
        .execute(pool)
        .await?;
    Ok(())
}

/// Queue invalidations in the transaction that changes the right. No remote call belongs here.
pub async fn enqueue_user_revocations(
    conn: &mut PgConnection,
    org_id: Uuid,
    user_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,$2,$3,$4,$4 FROM figcraft_links l JOIN figcraft_live_files f ON f.file_id=l.file_id WHERE f.org_id=$1 AND l.user_id=$2 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(user_id).bind(reason).bind(now).execute(conn).await?;
    Ok(())
}

/// App whose assignment governs links of unknown (`legacy`) origin.
const LEGACY_LINK_APP: &str = "figcraft";

/// Queue capabilities from one app for one user. Browser, launcher, and CLI
/// links remain untouched; unknown-origin legacy links are queued only when
/// the removed app is FigCraft.
pub async fn enqueue_user_app_revocations(
    conn: &mut PgConnection,
    org_id: Uuid,
    user_id: Uuid,
    app: &str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let reason = format!("app_access_revoked:app:{app}");
    let legacy_reason = "app_access_revoked:legacy";
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) \
        SELECT DISTINCT l.file_id,l.user_id,$3,$4,$4 \
        FROM figcraft_links l JOIN figcraft_live_files lf ON lf.file_id=l.file_id \
        WHERE lf.org_id=$1 AND l.user_id=$2 AND l.revoked_at IS NULL AND l.source_kind='app' AND l.source_app=$5 \
        ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(user_id).bind(&reason).bind(now).bind(app).execute(&mut *conn).await?;
    // Links of unknown origin were issued by FigCraft, so they follow the
    // figcraft assignment and no other app.
    if app == LEGACY_LINK_APP {
        sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) \
            SELECT DISTINCT l.file_id,l.user_id,$3,$4,$4 \
            FROM figcraft_links l JOIN figcraft_live_files lf ON lf.file_id=l.file_id \
            WHERE lf.org_id=$1 AND l.user_id=$2 AND l.revoked_at IS NULL AND l.source_kind='legacy' \
            ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
            .bind(org_id).bind(user_id).bind(legacy_reason).bind(now).execute(conn).await?;
    }
    Ok(())
}

/// Queue app-origin links denied when an organization switches from all apps
/// to assigned apps. Unknown-origin legacy links are FigCraft links and are
/// queued for regular members without a figcraft assignment. Launcher and CLI
/// links of regular members lose the editing capability reserved for
/// assigned apps.
pub async fn enqueue_org_app_revocations(
    conn: &mut PgConnection,
    org_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) \
        SELECT DISTINCT l.file_id,l.user_id,'app_access_revoked:app:'||l.source_app,$2,$2 \
        FROM figcraft_links l JOIN figcraft_live_files lf ON lf.file_id=l.file_id \
        JOIN organization_members m ON m.org_id=lf.org_id AND m.user_id=l.user_id AND m.status='active' \
        WHERE lf.org_id=$1 AND l.source_kind='app' AND l.revoked_at IS NULL \
          AND m.role NOT IN ('owner','admin','guest') \
          AND NOT EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=lf.org_id AND a.user_id=l.user_id AND a.app=l.source_app) \
        ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(now).execute(&mut *conn).await?;
    // Legacy provenance means FigCraft: keep them for members who have it.
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) \
        SELECT DISTINCT l.file_id,l.user_id,'app_access_revoked:legacy',$2,$2 \
        FROM figcraft_links l JOIN figcraft_live_files lf ON lf.file_id=l.file_id \
        JOIN organization_members m ON m.org_id=lf.org_id AND m.user_id=l.user_id AND m.status='active' \
        WHERE lf.org_id=$1 AND l.source_kind='legacy' AND l.revoked_at IS NULL \
          AND m.role NOT IN ('owner','admin','guest') \
          AND NOT EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=lf.org_id AND a.user_id=l.user_id AND a.app='figcraft') \
        ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(now).execute(&mut *conn).await?;
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) \
        SELECT DISTINCT l.file_id,l.user_id,'app_access_revoked:tool:'||l.source_kind,$2,$2 \
        FROM figcraft_links l JOIN figcraft_live_files lf ON lf.file_id=l.file_id \
        JOIN organization_members m ON m.org_id=lf.org_id AND m.user_id=l.user_id AND m.status='active' \
        WHERE lf.org_id=$1 AND l.source_kind IN ('launcher','cli') AND l.revoked_at IS NULL \
          AND m.role NOT IN ('owner','admin','guest') \
        ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(now).execute(conn).await?;
    Ok(())
}

pub async fn enqueue_file_app_revocations_pool(
    pool: &PgPool,
    org_id: Uuid,
    file_id: Uuid,
    user_id: Uuid,
    source_app: Option<&str>,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let reason = source_app.map_or_else(
        || "app_access_revoked:legacy".to_owned(),
        |app| format!("app_access_revoked:app:{app}"),
    );
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) \
        SELECT DISTINCT l.file_id,l.user_id,$5,$4,$4 FROM figcraft_links l \
        JOIN figcraft_live_files lf ON lf.file_id=l.file_id \
        WHERE lf.org_id=$1 AND l.file_id=$2 AND l.user_id=$3 AND l.revoked_at IS NULL \
          AND (($6::text IS NULL AND l.source_kind='legacy') OR ($6::text IS NOT NULL AND l.source_kind='app' AND l.source_app=$6)) \
        ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(file_id).bind(user_id).bind(now).bind(reason).bind(source_app).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Queue the launcher or CLI links of one user on one file, e.g. when
/// reconciliation finds that the organization no longer allows them.
pub async fn enqueue_file_tool_revocations_pool(
    pool: &PgPool,
    file_id: Uuid,
    user_id: Uuid,
    source_kind: &str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    if !matches!(source_kind, "launcher" | "cli") {
        return Err(DbError::Internal);
    }
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) \
        SELECT DISTINCT l.file_id,l.user_id,'app_access_revoked:tool:'||l.source_kind,$4,$4 FROM figcraft_links l \
        WHERE l.file_id=$1 AND l.user_id=$2 AND l.source_kind=$3 AND l.revoked_at IS NULL \
        ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(file_id).bind(user_id).bind(source_kind).bind(now).execute(pool).await?;
    Ok(())
}

pub async fn enqueue_file_user_revocations(
    conn: &mut PgConnection,
    file_id: Uuid,
    user_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT file_id,$2,$3,$4,$4 FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(file_id).bind(user_id).bind(reason).bind(now).execute(conn).await?;
    Ok(())
}

pub async fn enqueue_project_user_revocations(
    conn: &mut PgConnection,
    org_id: Uuid,
    project_id: Uuid,
    user_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,$3,$4,$5,$5 FROM figcraft_links l JOIN files f ON f.id=l.file_id WHERE f.org_id=$1 AND f.project_id=$2 AND l.user_id=$3 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(project_id).bind(user_id).bind(reason).bind(now).execute(conn).await?;
    Ok(())
}

pub async fn enqueue_workspace_user_revocations(
    conn: &mut PgConnection,
    org_id: Uuid,
    workspace_id: Uuid,
    user_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,$3,$4,$5,$5 FROM figcraft_links l JOIN files f ON f.id=l.file_id WHERE f.org_id=$1 AND f.workspace_id=$2 AND l.user_id=$3 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(workspace_id).bind(user_id).bind(reason).bind(now).execute(conn).await?;
    Ok(())
}

pub async fn enqueue_file_revocation(
    conn: &mut PgConnection,
    file_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let _ = reason;
    sqlx::query("UPDATE figcraft_live_files SET state='unpublishing',final_snapshot_done=false,last_snapshot_attempt_at=NULL,updated_at=$2 WHERE file_id=$1 AND state='live'").bind(file_id).bind(now).execute(&mut *conn).await?;
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,l.user_id,'unpublish',$2,$2 FROM figcraft_links l WHERE l.file_id=$1 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(file_id).bind(now).execute(conn).await?;
    Ok(())
}

pub async fn enqueue_all_user_revocations(
    conn: &mut PgConnection,
    user_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,$1,$2,$3,$3 FROM figcraft_links l WHERE l.user_id=$1 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(user_id).bind(reason).bind(now).execute(conn).await?;
    Ok(())
}

pub async fn enqueue_user_revocations_pool(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    enqueue_user_revocations(&mut tx, org_id, user_id, reason, now).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn enqueue_file_user_revocations_pool(
    pool: &PgPool,
    file_id: Uuid,
    user_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    enqueue_file_user_revocations(&mut tx, file_id, user_id, reason, now).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn enqueue_org_revocations(
    conn: &mut PgConnection,
    org_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,l.user_id,$2,$3,$3 FROM figcraft_links l JOIN figcraft_live_files f ON f.file_id=l.file_id WHERE f.org_id=$1 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(reason).bind(now).execute(conn).await?;
    Ok(())
}

pub async fn enqueue_project_revocations(
    conn: &mut PgConnection,
    org_id: Uuid,
    project_id: Uuid,
    reason: &'static str,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let _ = reason;
    sqlx::query("UPDATE figcraft_live_files l SET state='unpublishing',final_snapshot_done=false,last_snapshot_attempt_at=NULL,updated_at=$3 FROM files f WHERE l.file_id=f.id AND f.org_id=$1 AND f.project_id=$2 AND l.state='live'").bind(org_id).bind(project_id).bind(now).execute(&mut *conn).await?;
    sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,l.user_id,'unpublish',$3,$3 FROM figcraft_links l JOIN files f ON f.id=l.file_id WHERE f.org_id=$1 AND f.project_id=$2 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)")
        .bind(org_id).bind(project_id).bind(now).execute(conn).await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct Revocation {
    pub id: Uuid,
    pub file_id: Uuid,
    pub user_id: Option<Uuid>,
    pub reason: String,
    pub attempts: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRevocationSource {
    Legacy,
    App(String),
    /// Launcher or CLI links (`launcher` or `cli`).
    Tool(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAppRevocationReason;

impl std::fmt::Display for InvalidAppRevocationReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid app revocation reason")
    }
}

impl std::error::Error for InvalidAppRevocationReason {}

/// `None` is an ordinary all-source revocation; malformed app reasons fail closed.
pub fn parse_app_revocation_reason(
    reason: &str,
) -> Result<Option<AppRevocationSource>, InvalidAppRevocationReason> {
    let Some(source) = reason.strip_prefix("app_access_revoked:") else {
        return if reason.starts_with("app_access_revoked") {
            Err(InvalidAppRevocationReason)
        } else {
            Ok(None)
        };
    };
    if source == "legacy" {
        return Ok(Some(AppRevocationSource::Legacy));
    }
    if let Some(kind) = source.strip_prefix("tool:") {
        return if matches!(kind, "launcher" | "cli") {
            Ok(Some(AppRevocationSource::Tool(kind.to_owned())))
        } else {
            Err(InvalidAppRevocationReason)
        };
    }
    let Some(app) = source.strip_prefix("app:") else {
        return Err(InvalidAppRevocationReason);
    };
    if app.is_empty()
        || app.len() > 32
        || !app
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(InvalidAppRevocationReason);
    }
    Ok(Some(AppRevocationSource::App(app.to_owned())))
}

pub async fn claim_revocations(
    pool: &PgPool,
    limit: i64,
    now: OffsetDateTime,
) -> Result<Vec<Revocation>, DbError> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query_as::<_, Revocation>("WITH picked AS (SELECT id FROM figcraft_outbox WHERE (claimed_at IS NULL OR claimed_at<$1-interval '1 minute') AND available_at<=$1 ORDER BY available_at,created_at,id FOR UPDATE SKIP LOCKED LIMIT $2) UPDATE figcraft_outbox o SET claimed_at=$1 FROM picked WHERE o.id=picked.id RETURNING o.id,o.file_id,o.user_id,o.reason,o.attempts")
        .bind(now).bind(limit.max(1)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn app_access_revocation_links(
    pool: &PgPool,
    file_id: Uuid,
    user_id: Uuid,
    source_app: Option<&str>,
) -> Result<Vec<LiveLink>, DbError> {
    Ok(sqlx::query_as("SELECT id,file_id,user_id,access,figcraft_link_id,token_ciphertext,created_at,revoked_at FROM figcraft_links \
        WHERE file_id=$1 AND user_id=$2 AND revoked_at IS NULL \
          AND (($3::text IS NULL AND source_kind='legacy') OR ($3::text IS NOT NULL AND source_kind='app' AND source_app=$3)) ORDER BY id")
        .bind(file_id).bind(user_id).bind(source_app).fetch_all(pool).await?)
}

pub async fn tool_revocation_links(
    pool: &PgPool,
    file_id: Uuid,
    user_id: Uuid,
    source_kind: &str,
) -> Result<Vec<LiveLink>, DbError> {
    Ok(sqlx::query_as("SELECT id,file_id,user_id,access,figcraft_link_id,token_ciphertext,created_at,revoked_at FROM figcraft_links \
        WHERE file_id=$1 AND user_id=$2 AND revoked_at IS NULL AND source_kind=$3 AND source_kind IN ('launcher','cli') ORDER BY id")
        .bind(file_id).bind(user_id).bind(source_kind).fetch_all(pool).await?)
}

pub async fn finish_revocation(
    pool: &PgPool,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let reason: String =
        sqlx::query_scalar("SELECT reason FROM figcraft_outbox WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let source = parse_app_revocation_reason(&reason).map_err(|_| DbError::Internal)?;
    let rows: Vec<(Uuid, Uuid, Uuid, String)> = match source {
        None => sqlx::query_as("UPDATE figcraft_links SET revoked_at=$2 WHERE file_id=(SELECT file_id FROM figcraft_outbox WHERE id=$1) AND ((SELECT user_id FROM figcraft_outbox WHERE id=$1) IS NULL OR user_id=(SELECT user_id FROM figcraft_outbox WHERE id=$1)) AND revoked_at IS NULL RETURNING id,file_id,user_id,access")
            .bind(id).bind(now).fetch_all(&mut *tx).await?,
        Some(AppRevocationSource::Legacy) => sqlx::query_as("UPDATE figcraft_links SET revoked_at=$2 WHERE file_id=(SELECT file_id FROM figcraft_outbox WHERE id=$1) AND ((SELECT user_id FROM figcraft_outbox WHERE id=$1) IS NULL OR user_id=(SELECT user_id FROM figcraft_outbox WHERE id=$1)) AND source_kind='legacy' AND revoked_at IS NULL RETURNING id,file_id,user_id,access")
            .bind(id).bind(now).fetch_all(&mut *tx).await?,
        Some(AppRevocationSource::Tool(kind)) => sqlx::query_as("UPDATE figcraft_links SET revoked_at=$2 WHERE file_id=(SELECT file_id FROM figcraft_outbox WHERE id=$1) AND ((SELECT user_id FROM figcraft_outbox WHERE id=$1) IS NULL OR user_id=(SELECT user_id FROM figcraft_outbox WHERE id=$1)) AND source_kind=$3 AND revoked_at IS NULL RETURNING id,file_id,user_id,access")
            .bind(id).bind(now).bind(kind).fetch_all(&mut *tx).await?,
        Some(AppRevocationSource::App(app)) => sqlx::query_as("UPDATE figcraft_links SET revoked_at=$2 WHERE file_id=(SELECT file_id FROM figcraft_outbox WHERE id=$1) AND ((SELECT user_id FROM figcraft_outbox WHERE id=$1) IS NULL OR user_id=(SELECT user_id FROM figcraft_outbox WHERE id=$1)) AND source_kind='app' AND source_app=$3 AND revoked_at IS NULL RETURNING id,file_id,user_id,access")
            .bind(id).bind(now).bind(app).fetch_all(&mut *tx).await?,
    };
    for (link_id, file_id, user_id, access) in rows {
        let (org_id, workspace_id): (Uuid, Uuid) =
            sqlx::query_as("SELECT org_id,workspace_id FROM figcraft_live_files WHERE file_id=$1")
                .bind(file_id)
                .fetch_one(&mut *tx)
                .await?;
        let mut event = crate::audit::NewEvent::new("figcraft.link_revoked")
            .org(org_id)
            .target("file", file_id)
            .workspace(workspace_id);
        event
            .details
            .push(("link_id", crate::audit::Detail::Id(link_id)));
        event
            .details
            .push(("user_id", crate::audit::Detail::Id(user_id)));
        event.details.push((
            "access",
            crate::audit::Detail::Code(if access == "edit" { "edit" } else { "view" }),
        ));
        crate::audit::record_in(&mut tx, &crate::audit::AuditCtx::system(), &event).await?;
    }
    sqlx::query("DELETE FROM figcraft_outbox WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Deletes revoked capability rows revoked before `cutoff`; keeps the table bounded.
pub async fn prune_revoked_links(pool: &PgPool, cutoff: OffsetDateTime) -> Result<u64, DbError> {
    Ok(sqlx::query("DELETE FROM figcraft_links WHERE id IN (SELECT id FROM figcraft_links WHERE revoked_at IS NOT NULL AND revoked_at<$1 LIMIT 500)")
        .bind(cutoff).execute(pool).await?.rows_affected())
}

/// Reschedules a claimed item after local contention without counting a failed attempt.
pub async fn defer_revocation(
    pool: &PgPool,
    id: Uuid,
    now: OffsetDateTime,
    delay: time::Duration,
) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_outbox SET claimed_at=NULL,available_at=$2 WHERE id=$1")
        .bind(id)
        .bind(now + delay)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn retry_revocation(pool: &PgPool, id: Uuid, now: OffsetDateTime) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_outbox SET attempts=attempts+1,claimed_at=NULL,available_at=$2 + LEAST(300, (1 << LEAST(attempts,8))) * interval '1 second' WHERE id=$1")
        .bind(id).bind(now).execute(pool).await?;
    Ok(())
}

pub async fn mark_snapshot_attempt(
    pool: &PgPool,
    file_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_live_files SET last_snapshot_attempt_at=$2 WHERE file_id=$1 AND state='live'").bind(file_id).bind(now).execute(pool).await?;
    Ok(())
}

/// Records a final-snapshot attempt of an unpublishing file so failures back off.
pub async fn mark_final_snapshot_attempt(
    pool: &PgPool,
    file_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_live_files SET last_snapshot_attempt_at=$2 WHERE file_id=$1 AND state='unpublishing'").bind(file_id).bind(now).execute(pool).await?;
    Ok(())
}

pub async fn mark_snapshot_unchanged(
    pool: &PgPool,
    file_id: Uuid,
    version_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("UPDATE figcraft_live_files SET last_snapshot_version_id=$2,last_snapshot_at=$3,last_snapshot_attempt_at=$3,updated_at=$3 WHERE file_id=$1 AND state='live'").bind(file_id).bind(version_id).bind(now).execute(pool).await?;
    Ok(())
}

impl OrgTx {
    pub async fn figcraft_mark_unpublishing(
        &mut self,
        scope: &OrgScope,
        file_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        use craftcloud_core::policy::Action;
        self.check(scope, Action::FileLockForce)?;
        let changed=sqlx::query("UPDATE figcraft_live_files SET state='unpublishing',final_snapshot_done=false,last_snapshot_attempt_at=NULL,updated_at=$3 WHERE org_id=$1 AND file_id=$2 AND state='live'").bind(self.org_id).bind(file_id).bind(now).execute(&mut *self.tx).await?.rows_affected()==1;
        if changed {
            sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) SELECT DISTINCT l.file_id,l.user_id,'unpublish',$2,$2 FROM figcraft_links l WHERE l.file_id=$1 AND l.revoked_at IS NULL ON CONFLICT(file_id,user_id,reason) DO UPDATE SET available_at=LEAST(figcraft_outbox.available_at,EXCLUDED.available_at)").bind(file_id).bind(now).execute(&mut *self.tx).await?;
        }
        Ok(changed)
    }

    /// Rechecks native file head and creates the publish intent under the organization lock.
    pub async fn figcraft_begin_publish(
        &mut self,
        scope: &OrgScope,
        file_id: Uuid,
        workspace_id: Uuid,
        expected_head: Uuid,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<PublishIntentOutcome, DbError> {
        use craftcloud_core::policy::Action;
        self.check(scope, Action::FileRead)?;
        let row:Option<(Uuid,Option<Uuid>,bool,bool)>=sqlx::query_as("SELECT f.workspace_id,f.head_version_id,f.deleted_at IS NOT NULL,p.deleted_at IS NOT NULL FROM files f JOIN projects p ON p.id=f.project_id WHERE f.org_id=$1 AND f.id=$2 FOR UPDATE OF f,p").bind(self.org_id).bind(file_id).fetch_optional(&mut *self.tx).await?;
        let Some((actual_workspace, head, file_deleted, project_deleted)) = row else {
            return Ok(PublishIntentOutcome::Gone);
        };
        if actual_workspace != workspace_id || file_deleted || project_deleted {
            return Ok(PublishIntentOutcome::Gone);
        }
        if head != Some(expected_head) {
            return Ok(PublishIntentOutcome::HeadChanged);
        }
        let result=sqlx::query("INSERT INTO figcraft_live_files(file_id,org_id,workspace_id,published_from_version_id,state,published_by_user_id,created_at,updated_at) VALUES($1,$2,$3,$4,'publishing',$5,$6,$6) ON CONFLICT(file_id) DO UPDATE SET org_id=EXCLUDED.org_id,workspace_id=EXCLUDED.workspace_id,published_from_version_id=EXCLUDED.published_from_version_id,published_by_user_id=EXCLUDED.published_by_user_id,state='publishing',figcraft_file_id=NULL,final_snapshot_done=false,updated_at=EXCLUDED.updated_at WHERE figcraft_live_files.state='republish'")
            .bind(file_id).bind(self.org_id).bind(workspace_id).bind(expected_head).bind(user_id).bind(now).execute(&mut *self.tx).await?;
        Ok(if result.rows_affected() == 1 {
            PublishIntentOutcome::Started
        } else {
            PublishIntentOutcome::Busy
        })
    }

    /// Inserts and audits a capability only after current session and access have been rechecked.
    pub async fn figcraft_insert_link(
        &mut self,
        scope: &OrgScope,
        link: NewFigcraftLink<'_>,
        ctx: &crate::audit::AuditCtx,
    ) -> Result<bool, DbError> {
        use craftcloud_core::policy::Action;
        self.check(scope, Action::FileRead)?;
        let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM figcraft_live_files WHERE file_id=$1 AND org_id=$2 AND workspace_id=$3 AND figcraft_file_id=$4 AND state='live')").bind(link.file_id).bind(self.org_id).bind(link.workspace_id).bind(link.remote_file).fetch_one(&mut *self.tx).await?;
        if !live {
            return Ok(false);
        }
        sqlx::query("INSERT INTO figcraft_links(id,file_id,user_id,access,figcraft_link_id,token_ciphertext,created_at,source_kind,source_app) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(link.id).bind(link.file_id).bind(link.user_id).bind(link.access).bind(link.remote_link).bind(link.ciphertext).bind(link.now).bind(link.source_kind).bind(link.source_app).execute(&mut *self.tx).await?;
        let event = crate::audit::NewEvent::new("figcraft.link_issued")
            .org(self.org_id)
            .target("file", link.file_id)
            .workspace(link.workspace_id);
        crate::audit::record_in(&mut self.tx, ctx, &event).await?;
        Ok(true)
    }

    pub async fn figcraft_link_active(
        &mut self,
        id: Uuid,
        file_id: Uuid,
        user_id: Uuid,
        access: &str,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM figcraft_links WHERE id=$1 AND file_id=$2 AND user_id=$3 AND access=$4 AND revoked_at IS NULL)")
            .bind(id).bind(file_id).bind(user_id).bind(access).fetch_one(&mut *self.tx).await?)
    }

    /// Builds an internal FigCraft scope only from the freshly locked live-file row.
    ///
    /// Lock order: the publisher's users row (`FOR NO KEY UPDATE`) before the
    /// live-file row. Account disabling locks the users row first and then
    /// takes a key-share lock on the live-file row through the outbox foreign
    /// key, and `upload_create` locks the same users row; taking the live row
    /// first would close a cycle with either of them.
    pub async fn figcraft_service_scope(
        &mut self,
        file_id: Uuid,
        action: craftcloud_core::policy::Action,
    ) -> Result<Option<OrgScope>, DbError> {
        use craftcloud_core::policy::{FigcraftFileState, FigcraftService, Resource, can};
        let publisher: Option<Uuid> = sqlx::query_scalar(
            "SELECT published_by_user_id FROM figcraft_live_files WHERE file_id=$1",
        )
        .bind(file_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some(publisher) = publisher else {
            return Ok(None);
        };
        sqlx::query("SELECT 1 FROM users WHERE id=$1 FOR NO KEY UPDATE")
            .bind(publisher)
            .execute(&mut *self.tx)
            .await?;
        let row: Option<(Uuid, Uuid, String, Uuid)> = sqlx::query_as(
            "SELECT org_id,workspace_id,state,published_by_user_id FROM figcraft_live_files WHERE file_id=$1 FOR UPDATE",
        )
        .bind(file_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((org_id, workspace_id, state, locked_publisher)) = row else {
            return Ok(None);
        };
        // A republish by someone else between the two reads: retry next pass.
        if locked_publisher != publisher {
            return Ok(None);
        }
        if org_id != self.org_id {
            return Ok(None);
        }
        let state = match state.as_str() {
            "live" => FigcraftFileState::Live,
            "unpublishing" => FigcraftFileState::Unpublishing,
            _ => return Ok(None),
        };
        let service = FigcraftService {
            org_id,
            workspace_id,
            file_id,
            state,
        };
        let resource = Resource::FigcraftLiveFile {
            org_id,
            workspace_id,
            file_id,
            state,
        };
        Ok(can(&service, action, &resource)
            .permit()
            .and_then(|permit| OrgScope::from_permit(&permit)))
    }

    pub async fn figcraft_record_snapshot(
        &mut self,
        file_id: Uuid,
        version_id: Uuid,
        now: OffsetDateTime,
        final_snapshot: bool,
    ) -> Result<bool, DbError> {
        let changed = sqlx::query("UPDATE figcraft_live_files SET last_snapshot_version_id=$3,last_snapshot_at=$4,last_snapshot_attempt_at=$4,final_snapshot_done=final_snapshot_done OR $5,updated_at=$4 WHERE org_id=$1 AND file_id=$2 AND state IN ('live','unpublishing')")
            .bind(self.org_id).bind(file_id).bind(version_id).bind(now).bind(final_snapshot).execute(&mut *self.tx).await?.rows_affected() == 1;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_revocation_reasons_parse_every_source_and_fail_closed() {
        assert_eq!(parse_app_revocation_reason("grant_revoked"), Ok(None));
        assert_eq!(
            parse_app_revocation_reason("app_access_revoked:legacy"),
            Ok(Some(AppRevocationSource::Legacy))
        );
        assert_eq!(
            parse_app_revocation_reason("app_access_revoked:app:figcraft"),
            Ok(Some(AppRevocationSource::App("figcraft".to_owned())))
        );
        for kind in ["launcher", "cli"] {
            assert_eq!(
                parse_app_revocation_reason(&format!("app_access_revoked:tool:{kind}")),
                Ok(Some(AppRevocationSource::Tool(kind.to_owned())))
            );
        }
        for bad in [
            "app_access_revoked",
            "app_access_revoked:tool:browser",
            "app_access_revoked:tool:",
            "app_access_revoked:app:",
        ] {
            assert_eq!(
                parse_app_revocation_reason(bad),
                Err(InvalidAppRevocationReason),
                "{bad}"
            );
        }
    }
}
