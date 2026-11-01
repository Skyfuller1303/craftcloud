//! Tenant-scoped SCIM persistence.
//!
//! Every mutation locks its organization before validating the current token.

use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;

/// Begin serialized SCIM transaction after rechecking the bearer hash.
///
/// Returns `None` for missing organizations and invalid/revoked tokens. This
/// deliberately keeps those cases indistinguishable to SCIM callers.
pub async fn begin(
    pool: &PgPool,
    org_id: Uuid,
    bearer: &str,
) -> Result<Option<Transaction<'static, Postgres>>, DbError> {
    let mut tx = pool.begin().await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE id=$1 FOR UPDATE)")
            .bind(org_id)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Ok(None);
    }
    let hash = Sha256::digest(bearer.as_bytes());
    let valid: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM scim_tokens WHERE org_id=$1 AND token_hash=$2)",
    )
    .bind(org_id)
    .bind(hash.as_slice())
    .fetch_one(&mut *tx)
    .await?;
    if !valid {
        return Ok(None);
    }
    Ok(Some(tx))
}

/// SHA-256 digest for storage; plaintext token must never be persisted.
#[must_use]
pub fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Insert or rotate an organization's token hash. Caller must hold org lock.
pub async fn rotate_token(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    hash: &[u8; 32],
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO scim_tokens(org_id,token_hash) VALUES($1,$2) ON CONFLICT(org_id) DO UPDATE SET token_hash=EXCLUDED.token_hash,created_at=now()")
        .bind(org_id).bind(hash.as_slice()).execute(&mut **tx).await?;
    Ok(())
}

/// Remove an organization's active token. Caller must hold org lock.
pub async fn revoke_token(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
) -> Result<bool, DbError> {
    Ok(sqlx::query("DELETE FROM scim_tokens WHERE org_id=$1")
        .bind(org_id)
        .execute(&mut **tx)
        .await?
        .rows_affected()
        == 1)
}

/// A tenant-scoped SCIM user and its global account identity.
#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub org_id: Uuid,
    pub id: Uuid,
    pub user_id: Uuid,
    pub external_id: Option<String>,
    pub user_name: String,
    pub display_name: String,
    pub active: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Values accepted when creating or replacing a SCIM user.
#[derive(Clone)]
pub struct UserInput {
    pub user_name: String,
    pub display_name: String,
    pub external_id: Option<String>,
    pub active: bool,
}

const USER_SELECT: &str = "SELECT org_id,id,user_id,external_id,user_name,display_name,active,created_at,updated_at FROM scim_users";

/// Read one SCIM user in the given organization.
pub async fn get_user(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    id: Uuid,
) -> Result<Option<User>, DbError> {
    Ok(
        sqlx::query_as::<_, User>(&format!("{USER_SELECT} WHERE org_id=$1 AND id=$2"))
            .bind(org_id)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?,
    )
}

/// List SCIM users ordered by id, with an exact `userName` filter.
pub async fn list_users(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    filter: Option<&str>,
    start: i64,
    count: i64,
) -> Result<(i64, Vec<User>), DbError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM scim_users WHERE org_id=$1 AND ($2::text IS NULL OR lower(user_name)=lower($2))",
    )
    .bind(org_id)
    .bind(filter)
    .fetch_one(&mut **tx)
    .await?;
    let start = start.max(1).saturating_sub(1);
    let users = sqlx::query_as::<_, User>(&format!("{USER_SELECT} WHERE org_id=$1 AND ($2::text IS NULL OR lower(user_name)=lower($2)) ORDER BY id OFFSET $3 LIMIT $4"))
        .bind(org_id).bind(filter).bind(start).bind(count.clamp(0, 200)).fetch_all(&mut **tx).await?;
    Ok((total, users))
}

async fn lock_org(tx: &mut Transaction<'_, Postgres>, org_id: Uuid) -> Result<(), DbError> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE id=$1 FOR UPDATE)")
            .bind(org_id)
            .fetch_one(&mut **tx)
            .await?;
    if !exists {
        return Err(DbError::CheckViolation {
            constraint: Some("scim_org_not_found".to_owned()),
        });
    }
    Ok(())
}

async fn protected_user(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT COALESCE(u.instance_role='operator',false) OR COALESCE(m.role='owner',false) FROM users u LEFT JOIN organization_members m ON m.user_id=u.id AND m.org_id=$2 WHERE u.id=$1")
        .bind(user_id).bind(org_id).fetch_optional(&mut **tx).await?.unwrap_or(false))
}

async fn set_active(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    user_id: Uuid,
    active: bool,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    if active {
        sqlx::query(
            "UPDATE organization_members SET status='active' WHERE org_id=$1 AND user_id=$2",
        )
        .bind(org_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE organization_members SET status='disabled' WHERE org_id=$1 AND user_id=$2",
        )
        .bind(org_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query("INSERT INTO organization_session_cutoffs(org_id,user_id,revoked_before) VALUES($1,$2,$3) ON CONFLICT(org_id,user_id) DO UPDATE SET revoked_before=GREATEST(organization_session_cutoffs.revoked_before,EXCLUDED.revoked_before)")
            .bind(org_id).bind(user_id).bind(now).execute(&mut **tx).await?;
        sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND holder_user_id=$2")
            .bind(org_id)
            .bind(user_id)
            .execute(&mut **tx)
            .await?;
        crate::figcraft::enqueue_user_revocations(tx, org_id, user_id, "membership_removed", now)
            .await?;
    }
    Ok(())
}

/// Create an isolated global account and its member-only organization membership.
pub async fn create_user(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    input: &UserInput,
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<User, DbError> {
    lock_org(tx, org_id).await?;
    ensure_capacity(tx, org_id, input.active, now).await?;
    let user_id = Uuid::new_v4();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,normalized_email,display_name) VALUES($1,$2,$3)")
        .bind(user_id)
        .bind(&input.user_name)
        .bind(&input.display_name)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member',$3)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(if input.active { "active" } else { "disabled" })
    .execute(&mut **tx)
    .await?;
    let user = sqlx::query_as::<_, User>("INSERT INTO scim_users(org_id,id,user_id,external_id,user_name,display_name,active,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$8) RETURNING org_id,id,user_id,external_id,user_name,display_name,active,created_at,updated_at")
        .bind(org_id).bind(id).bind(user_id).bind(&input.external_id).bind(&input.user_name).bind(&input.display_name).bind(input.active).bind(now).fetch_one(&mut **tx).await?;
    if !input.active {
        set_active(tx, org_id, user_id, false, now).await?;
    }
    crate::audit::record_in(
        tx,
        ctx,
        &crate::audit::NewEvent::new("scim.user_created")
            .org(org_id)
            .target("scim_user", id),
    )
    .await?;
    Ok(user)
}

/// Replace a SCIM user without changing account verification or global identity.
pub async fn replace_user(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    id: Uuid,
    input: &UserInput,
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<Option<User>, DbError> {
    lock_org(tx, org_id).await?;
    let Some(current) = get_user(tx, org_id, id).await? else {
        return Ok(None);
    };
    if protected_user(tx, org_id, current.user_id).await? {
        return Err(DbError::CheckViolation {
            constraint: Some("protected_account".to_owned()),
        });
    }
    if input.active && !current.active {
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM organization_members WHERE org_id=$1 AND user_id=$2",
        )
        .bind(org_id)
        .bind(current.user_id)
        .fetch_optional(&mut **tx)
        .await?;
        if role.as_deref() != Some("guest") {
            ensure_capacity(tx, org_id, true, now).await?;
        }
    }
    if input.external_id != current.external_id {
        let bound: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM external_identities WHERE org_id=$1 AND user_id=$2)",
        )
        .bind(org_id)
        .bind(current.user_id)
        .fetch_one(&mut **tx)
        .await?;
        if bound {
            return Err(DbError::CheckViolation {
                constraint: Some("scim_bound_external_id".to_owned()),
            });
        }
    }
    // Directory attributes are tenant-scoped. Changing them must never transfer
    // ownership of an instance-wide mailbox or alter another tenant's identity.
    sqlx::query("UPDATE scim_users SET external_id=$3,user_name=$4,display_name=$5,active=$6,updated_at=$7 WHERE org_id=$1 AND id=$2")
        .bind(org_id).bind(id).bind(&input.external_id).bind(&input.user_name).bind(&input.display_name).bind(input.active).bind(now).execute(&mut **tx).await?;
    if current.active != input.active {
        set_active(tx, org_id, current.user_id, input.active, now).await?;
    }
    if current.active != input.active {
        reconcile_users(tx, org_id, &[current.user_id], None, now).await?;
    }
    let updated = get_user(tx, org_id, id).await?;
    crate::audit::record_in(
        tx,
        ctx,
        &crate::audit::NewEvent::new("scim.user_updated")
            .org(org_id)
            .target("scim_user", id),
    )
    .await?;
    Ok(updated)
}

/// Deactivate the tenant membership and remove only its SCIM resource.
pub async fn delete_user(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    id: Uuid,
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<bool, DbError> {
    lock_org(tx, org_id).await?;
    let Some(current) = get_user(tx, org_id, id).await? else {
        return Ok(false);
    };
    if protected_user(tx, org_id, current.user_id).await? {
        return Err(DbError::CheckViolation {
            constraint: Some("protected_account".to_owned()),
        });
    }
    set_active(tx, org_id, current.user_id, false, now).await?;
    reconcile_users(tx, org_id, &[current.user_id], None, now).await?;
    sqlx::query("DELETE FROM scim_users WHERE org_id=$1 AND id=$2")
        .bind(org_id)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    crate::audit::record_in(
        tx,
        ctx,
        &crate::audit::NewEvent::new("scim.user_deleted")
            .org(org_id)
            .target("scim_user", id),
    )
    .await?;
    Ok(true)
}

/// A SCIM group and its tenant-owned resource metadata.
#[derive(Debug, Clone, FromRow)]
pub struct Group {
    pub id: Uuid,
    pub org_id: Uuid,
    pub external_id: Option<String>,
    pub display_name: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Values accepted when creating or replacing a SCIM group.
#[derive(Clone)]
pub struct GroupInput {
    pub external_id: Option<String>,
    pub display_name: String,
    pub members: Vec<Uuid>,
}

const GROUP_SELECT: &str =
    "SELECT id,org_id,external_id,display_name,created_at,updated_at FROM scim_groups";

async fn group_members(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    id: Uuid,
) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar("SELECT scim_user_id FROM scim_group_members WHERE org_id=$1 AND group_id=$2 ORDER BY scim_user_id")
        .bind(org_id).bind(id).fetch_all(&mut **tx).await?)
}

async fn member_user_ids(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    members: &[Uuid],
) -> Result<Vec<Uuid>, DbError> {
    if members.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(
        "SELECT user_id FROM scim_users WHERE org_id=$1 AND id=ANY($2) ORDER BY user_id",
    )
    .bind(org_id)
    .bind(members)
    .fetch_all(&mut **tx)
    .await?)
}

async fn validate_group_input(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    input: &GroupInput,
) -> Result<Vec<Uuid>, DbError> {
    if input.members.len() > 100 {
        return Err(DbError::CheckViolation {
            constraint: Some("scim_group_member_limit".to_owned()),
        });
    }
    let members: std::collections::BTreeSet<_> = input.members.iter().copied().collect();
    if members.len() != input.members.len() {
        return Err(DbError::CheckViolation {
            constraint: Some("scim_group_duplicate_member".to_owned()),
        });
    }
    if !members.is_empty() {
        let found: i64 =
            sqlx::query_scalar("SELECT count(*) FROM scim_users WHERE org_id=$1 AND id=ANY($2)")
                .bind(org_id)
                .bind(members.iter().copied().collect::<Vec<_>>())
                .fetch_one(&mut **tx)
                .await?;
        if found != members.len() as i64 {
            return Err(DbError::CheckViolation {
                constraint: Some("scim_group_member_not_found".to_owned()),
            });
        }
    }
    Ok(members.into_iter().collect())
}

/// Read a group and ordered member SCIM user ids in one organization.
pub async fn get_group(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    id: Uuid,
) -> Result<Option<(Group, Vec<Uuid>)>, DbError> {
    let group = sqlx::query_as::<_, Group>(&format!("{GROUP_SELECT} WHERE org_id=$1 AND id=$2"))
        .bind(org_id)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    match group {
        Some(group) => Ok(Some((group, group_members(tx, org_id, id).await?))),
        None => Ok(None),
    }
}

/// List groups by display name, with an exact `displayName` filter.
pub async fn list_groups(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    filter: Option<&str>,
    start: i64,
    count: i64,
) -> Result<(i64, Vec<(Group, Vec<Uuid>)>), DbError> {
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM scim_groups WHERE org_id=$1 AND ($2::text IS NULL OR lower(display_name)=lower($2))")
        .bind(org_id).bind(filter).fetch_one(&mut **tx).await?;
    let groups = sqlx::query_as::<_, Group>(&format!("{GROUP_SELECT} WHERE org_id=$1 AND ($2::text IS NULL OR lower(display_name)=lower($2)) ORDER BY id OFFSET $3 LIMIT $4"))
        .bind(org_id).bind(filter).bind(start.max(1).saturating_sub(1)).bind(count.clamp(0, 200)).fetch_all(&mut **tx).await?;
    let mut result = Vec::with_capacity(groups.len());
    for group in groups {
        let members = group_members(tx, org_id, group.id).await?;
        result.push((group, members));
    }
    Ok((total, result))
}

async fn set_group_members(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    group_id: Uuid,
    members: &[Uuid],
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM scim_group_members WHERE org_id=$1 AND group_id=$2")
        .bind(org_id)
        .bind(group_id)
        .execute(&mut **tx)
        .await?;
    for user_id in members {
        sqlx::query(
            "INSERT INTO scim_group_members(org_id,group_id,scim_user_id) VALUES($1,$2,$3)",
        )
        .bind(org_id)
        .bind(group_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Create a group and its tenant-scoped member set.
pub async fn create_group(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    input: &GroupInput,
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<(Group, Vec<Uuid>), DbError> {
    lock_org(tx, org_id).await?;
    let members = validate_group_input(tx, org_id, input).await?;
    let id = Uuid::new_v4();
    let group = sqlx::query_as::<_, Group>("INSERT INTO scim_groups(org_id,id,external_id,display_name,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$5) RETURNING id,org_id,external_id,display_name,created_at,updated_at")
        .bind(org_id).bind(id).bind(&input.external_id).bind(&input.display_name).bind(now).fetch_one(&mut **tx).await?;
    set_group_members(tx, org_id, id, &members).await?;
    crate::audit::record_in(
        tx,
        ctx,
        &crate::audit::NewEvent::new("scim.group_created")
            .org(org_id)
            .target("scim_group", id),
    )
    .await?;
    Ok((group, members))
}

/// Replace a group and its complete member set.
pub async fn replace_group(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    id: Uuid,
    input: &GroupInput,
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<Option<(Group, Vec<Uuid>)>, DbError> {
    lock_org(tx, org_id).await?;
    let Some((_, previous_members)) = get_group(tx, org_id, id).await? else {
        return Ok(None);
    };
    let mut affected = member_user_ids(tx, org_id, &previous_members).await?;
    let members = validate_group_input(tx, org_id, input).await?;
    affected.extend(member_user_ids(tx, org_id, &members).await?);
    let group = sqlx::query_as::<_, Group>("UPDATE scim_groups SET external_id=$3,display_name=$4,updated_at=$5 WHERE org_id=$1 AND id=$2 RETURNING id,org_id,external_id,display_name,created_at,updated_at")
        .bind(org_id).bind(id).bind(&input.external_id).bind(&input.display_name).bind(now).fetch_one(&mut **tx).await?;
    set_group_members(tx, org_id, id, &members).await?;
    reconcile_users(tx, org_id, &affected, None, now).await?;
    crate::audit::record_in(
        tx,
        ctx,
        &crate::audit::NewEvent::new("scim.group_updated")
            .org(org_id)
            .target("scim_group", id),
    )
    .await?;
    Ok(Some((group, members)))
}

/// Delete a group and reconcile memberships it previously supplied.
pub async fn delete_group(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    id: Uuid,
    _now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<bool, DbError> {
    lock_org(tx, org_id).await?;
    let Some((_, members)) = get_group(tx, org_id, id).await? else {
        return Ok(false);
    };
    let affected = member_user_ids(tx, org_id, &members).await?;
    sqlx::query("DELETE FROM scim_groups WHERE org_id=$1 AND id=$2")
        .bind(org_id)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    reconcile_users(tx, org_id, &affected, None, _now).await?;
    crate::audit::record_in(
        tx,
        ctx,
        &crate::audit::NewEvent::new("scim.group_deleted")
            .org(org_id)
            .target("scim_group", id),
    )
    .await?;
    Ok(true)
}

/// A workspace role granted through a SCIM group.
#[derive(Debug, Clone, FromRow)]
pub struct Mapping {
    pub group_id: Uuid,
    pub workspace_id: Uuid,
    pub role: String,
}

/// List a tenant's group-to-workspace mappings.
pub async fn list_mappings(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
) -> Result<Vec<Mapping>, DbError> {
    Ok(sqlx::query_as("SELECT group_id,workspace_id,role FROM scim_group_mappings WHERE org_id=$1 ORDER BY group_id,workspace_id")
        .bind(org_id).fetch_all(&mut **tx).await?)
}

async fn reconcile_users(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    user_ids: &[Uuid],
    only_workspaces: Option<&[Uuid]>,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let ids: Vec<_> = user_ids
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    if ids.len() > 1000 {
        return Err(DbError::CheckViolation {
            constraint: Some("scim_reconcile_user_limit".to_owned()),
        });
    }
    let targets: Vec<(Uuid, Uuid, Option<String>)> = sqlx::query_as("SELECT su.user_id, x.workspace_id, CASE WHEN bool_or(x.role='editor') THEN 'editor' ELSE 'viewer' END FROM scim_users su JOIN organization_members om ON om.org_id=su.org_id AND om.user_id=su.user_id AND om.status='active' AND om.role<>'guest' JOIN users u ON u.id=su.user_id AND u.disabled_at IS NULL JOIN scim_group_members gm ON gm.org_id=su.org_id AND gm.scim_user_id=su.id JOIN scim_group_mappings x ON x.org_id=gm.org_id AND x.group_id=gm.group_id WHERE su.org_id=$1 AND su.active AND su.user_id=ANY($2) AND ($3::uuid[] IS NULL OR x.workspace_id=ANY($3)) GROUP BY su.user_id,x.workspace_id")
        .bind(org_id).bind(&ids).bind(only_workspaces).fetch_all(&mut **tx).await?;
    let owned: Vec<(Uuid, Uuid, Uuid, String)> = sqlx::query_as("SELECT workspace_id,user_id,workspace_id,applied_role FROM scim_workspace_ownership WHERE org_id=$1 AND user_id=ANY($2) AND ($3::uuid[] IS NULL OR workspace_id=ANY($3))")
        .bind(org_id).bind(&ids).bind(only_workspaces).fetch_all(&mut **tx).await?;
    let mut pairs = std::collections::BTreeSet::new();
    for (user_id, workspace_id, _) in &targets {
        pairs.insert((*user_id, *workspace_id));
    }
    for (_, user_id, workspace_id, _) in &owned {
        pairs.insert((*user_id, *workspace_id));
    }
    for (user_id, workspace_id) in pairs {
        let desired = targets
            .iter()
            .find(|(u, w, _)| *u == user_id && *w == workspace_id)
            .and_then(|(_, _, r)| r.as_deref());
        let record = owned
            .iter()
            .find(|(_, u, w, _)| *u == user_id && *w == workspace_id);
        let actual: Option<String> = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(org_id)
        .bind(workspace_id)
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((_, _, _, applied)) = record {
            if actual.as_deref() != Some(applied.as_str()) {
                sqlx::query("DELETE FROM scim_workspace_ownership WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3").bind(org_id).bind(workspace_id).bind(user_id).execute(&mut **tx).await?;
                continue;
            }
        } else if actual.is_some() {
            continue;
        }
        match desired {
            Some(role) => {
                if actual.as_deref() == Some("editor") && role == "viewer" {
                    sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND holder_user_id=$2 AND file_id IN (SELECT id FROM files WHERE workspace_id=$3)")
                        .bind(org_id).bind(user_id).bind(workspace_id).execute(&mut **tx).await?;
                    crate::figcraft::enqueue_user_revocations(
                        tx,
                        org_id,
                        user_id,
                        "membership_removed",
                        now,
                    )
                    .await?;
                }
                sqlx::query("INSERT INTO workspace_members(org_id,workspace_id,user_id,role) VALUES($1,$2,$3,$4) ON CONFLICT(workspace_id,user_id) DO UPDATE SET role=EXCLUDED.role WHERE workspace_members.org_id=EXCLUDED.org_id")
                    .bind(org_id).bind(workspace_id).bind(user_id).bind(role).execute(&mut **tx).await?;
                sqlx::query("INSERT INTO scim_workspace_ownership(org_id,workspace_id,user_id,applied_role) VALUES($1,$2,$3,$4) ON CONFLICT(org_id,workspace_id,user_id) DO UPDATE SET applied_role=EXCLUDED.applied_role")
                    .bind(org_id).bind(workspace_id).bind(user_id).bind(role).execute(&mut **tx).await?;
            }
            None => {
                if actual.is_some() {
                    sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND holder_user_id=$2 AND file_id IN (SELECT id FROM files WHERE workspace_id=$3)")
                        .bind(org_id).bind(user_id).bind(workspace_id).execute(&mut **tx).await?;
                    crate::figcraft::enqueue_user_revocations(
                        tx,
                        org_id,
                        user_id,
                        "membership_removed",
                        now,
                    )
                    .await?;
                    sqlx::query("DELETE FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3 AND role=$4").bind(org_id).bind(workspace_id).bind(user_id).bind(record.map(|x|x.3.as_str()).unwrap_or("" )).execute(&mut **tx).await?;
                }
                sqlx::query("DELETE FROM scim_workspace_ownership WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3").bind(org_id).bind(workspace_id).bind(user_id).execute(&mut **tx).await?;
            }
        }
    }
    Ok(())
}

/// Replace mappings and reconcile all affected users while preserving manual roles.
pub async fn replace_mappings(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    mappings: &[Mapping],
    now: OffsetDateTime,
    ctx: &crate::audit::AuditCtx,
) -> Result<(), DbError> {
    lock_org(tx, org_id).await?;
    if mappings.len() > 100 {
        return Err(DbError::CheckViolation {
            constraint: Some("scim_mapping_limit".to_owned()),
        });
    }
    let unique: std::collections::BTreeSet<_> = mappings
        .iter()
        .map(|m| (m.group_id, m.workspace_id))
        .collect();
    if unique.len() != mappings.len()
        || mappings
            .iter()
            .any(|m| !matches!(m.role.as_str(), "viewer" | "editor"))
    {
        return Err(DbError::CheckViolation {
            constraint: Some("scim_mapping_invalid".to_owned()),
        });
    }
    let before = list_mappings(tx, org_id).await?;
    let mut workspaces: Vec<_> = before
        .iter()
        .chain(mappings)
        .map(|m| m.workspace_id)
        .collect();
    workspaces.sort_unstable();
    workspaces.dedup();
    if !mappings.is_empty() {
        let invalid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM unnest($2::uuid[],$3::uuid[]) AS x(group_id,workspace_id) WHERE NOT EXISTS(SELECT 1 FROM scim_groups g JOIN workspaces w ON w.org_id=g.org_id AND w.id=x.workspace_id WHERE g.org_id=$1 AND g.id=x.group_id))")
            .bind(org_id).bind(mappings.iter().map(|m|m.group_id).collect::<Vec<_>>()).bind(mappings.iter().map(|m|m.workspace_id).collect::<Vec<_>>()).fetch_one(&mut **tx).await?;
        if invalid {
            return Err(DbError::CheckViolation {
                constraint: Some("scim_mapping_scope_invalid".to_owned()),
            });
        }
    }
    let users: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM (SELECT DISTINCT su.user_id FROM scim_users su JOIN scim_group_members gm ON gm.org_id=su.org_id AND gm.scim_user_id=su.id WHERE su.org_id=$1 AND (gm.group_id=ANY($2) OR EXISTS(SELECT 1 FROM scim_group_mappings sm WHERE sm.org_id=gm.org_id AND sm.group_id=gm.group_id)) UNION SELECT user_id FROM scim_workspace_ownership WHERE org_id=$1) affected LIMIT 1001")
        .bind(org_id).bind(mappings.iter().map(|m|m.group_id).collect::<Vec<_>>()).fetch_all(&mut **tx).await?;
    if users.len() > 1000 {
        return Err(DbError::CheckViolation {
            constraint: Some("scim_reconcile_user_limit".to_owned()),
        });
    }
    sqlx::query("DELETE FROM scim_group_mappings WHERE org_id=$1")
        .bind(org_id)
        .execute(&mut **tx)
        .await?;
    for mapping in mappings {
        sqlx::query("INSERT INTO scim_group_mappings(org_id,group_id,workspace_id,role) VALUES($1,$2,$3,$4)")
            .bind(org_id).bind(mapping.group_id).bind(mapping.workspace_id).bind(&mapping.role).execute(&mut **tx).await?;
    }
    reconcile_users(tx, org_id, &users, Some(&workspaces), now).await?;
    crate::audit::record_in(
        tx,
        ctx,
        &crate::audit::NewEvent::new("scim.mappings_updated").org(org_id),
    )
    .await?;
    Ok(())
}

async fn ensure_capacity(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    active: bool,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let limit: Option<i32> = sqlx::query_scalar("SELECT seat_limit FROM organizations WHERE id=$1")
        .bind(org_id)
        .fetch_one(&mut **tx)
        .await?;
    if active && let Some(limit) = limit {
        let occupied: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM organization_members m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.status='active' AND m.role<>'guest' AND u.disabled_at IS NULL) + (SELECT count(*) FROM invitations i LEFT JOIN users u ON u.normalized_email=i.normalized_email LEFT JOIN organization_members m ON m.org_id=i.org_id AND m.user_id=u.id WHERE i.org_id=$1 AND i.org_role<>'guest' AND i.accepted_at IS NULL AND i.revoked_at IS NULL AND i.invalidated_at IS NULL AND i.expires_at>$2 AND (m.user_id IS NULL OR m.status<>'active'))")
            .bind(org_id).bind(now).fetch_one(&mut **tx).await?;
        if occupied >= i64::from(limit) {
            return Err(DbError::CheckViolation {
                constraint: Some("seat_limit".to_owned()),
            });
        }
    }
    Ok(())
}
