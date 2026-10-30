//! Tenant-scoped object grants.
use craftcloud_core::policy::{Action, ObjectTarget, Resource as PolicyResource};
use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::members::OrgTx;
use crate::{DbError, OrgScope, PgPool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantRole {
    Viewer,
    Editor,
}
fn policy_role(role: Option<String>) -> Option<craftcloud_core::policy::GrantRole> {
    role.as_deref()
        .and_then(GrantRole::parse)
        .map(|role| match role {
            GrantRole::Viewer => craftcloud_core::policy::GrantRole::Viewer,
            GrantRole::Editor => craftcloud_core::policy::GrantRole::Editor,
        })
}
impl GrantRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Editor => "editor",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "editor" => Some(Self::Editor),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Project,
    File,
}
impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::File => "file",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "project" => Some(Self::Project),
            "file" => Some(Self::File),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Grant {
    pub id: Uuid,
    pub org_id: Uuid,
    pub workspace_id: Uuid,
    pub target_kind: String,
    pub target_id: Uuid,
    pub grantee_user_id: Uuid,
    pub display_name: String,
    pub email: String,
    pub role: String,
    pub created_by_user_id: Uuid,
    pub created_at: OffsetDateTime,
    pub revoked_at: Option<OffsetDateTime>,
    pub version: i64,
}
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub org_id: Uuid,
    pub workspace_id: Uuid,
    pub kind: TargetKind,
    pub id: Uuid,
}
#[derive(Debug, Clone, Copy)]
pub struct NewGrant {
    pub target: Target,
    pub grantee_user_id: Uuid,
    pub role: GrantRole,
    pub created_by: Uuid,
    pub now: OffsetDateTime,
}
#[derive(Debug, Clone)]
pub enum CreateOutcome {
    Created(Box<Grant>),
    TargetGone,
    /// The file is in the trash.
    FileDeleted,
    /// The project (or the file's project) is in the trash.
    ProjectDeleted,
    NotMember,
    AlreadyExists,
}
#[derive(Debug, Clone)]
pub enum ChangeOutcome {
    Changed(Box<Grant>),
    Gone,
    VersionMismatch(i64),
}

#[derive(Debug, Clone, FromRow)]
pub struct SharedWithMeItem {
    pub created_at: OffsetDateTime,
    pub id: Uuid,
    pub target_kind: String,
    pub target_id: Uuid,
    pub role: String,
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub project_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub file_id: Option<Uuid>,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct OrganizationGrant {
    #[sqlx(flatten)]
    pub grant: Grant,
    pub target_name: String,
}

const SELECT: &str = "SELECT g.id,g.org_id,g.workspace_id,g.target_kind,g.target_id,g.grantee_user_id,u.display_name,u.normalized_email AS email,g.role,g.created_by_user_id,g.created_at,g.revoked_at,g.version FROM grants g JOIN users u ON u.id=g.grantee_user_id";

pub async fn locate(pool: &PgPool, id: Uuid) -> Result<Option<(Target, Uuid, i64, bool)>, DbError> {
    let row: Option<(Uuid, Uuid, String, Uuid, Uuid, i64, bool)> = sqlx::query_as("SELECT org_id,workspace_id,target_kind,target_id,grantee_user_id,version,revoked_at IS NOT NULL FROM grants WHERE id=$1")
        .bind(id).fetch_optional(pool).await?;
    Ok(row.and_then(
        |(org_id, workspace_id, kind, target_id, grantee, version, revoked)| {
            TargetKind::parse(&kind).map(|kind| {
                (
                    Target {
                        org_id,
                        workspace_id,
                        kind,
                        id: target_id,
                    },
                    grantee,
                    version,
                    revoked,
                )
            })
        },
    ))
}

pub async fn list(pool: &PgPool, scope: &OrgScope, target: Target) -> Result<Vec<Grant>, DbError> {
    Ok(sqlx::query_as::<_, Grant>(&format!("{SELECT} WHERE g.org_id=$1 AND g.workspace_id=$2 AND g.target_kind=$3 AND g.target_id=$4 AND g.revoked_at IS NULL ORDER BY g.created_at,g.id"))
        .bind(scope.org_id()).bind(target.workspace_id).bind(target.kind.as_str()).bind(target.id).fetch_all(pool).await?)
}

pub struct OrganizationGrantList<'a> {
    pub workspaces: &'a [Uuid],
    pub workspace_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub target_kind: Option<&'a str>,
    pub limit: u32,
    pub after: Option<(OffsetDateTime, Uuid)>,
}

pub async fn list_organization(
    pool: &PgPool,
    scope: &OrgScope,
    page: OrganizationGrantList<'_>,
) -> Result<Vec<OrganizationGrant>, DbError> {
    Ok(sqlx::query_as::<_, OrganizationGrant>(
        "SELECT g.id,g.org_id,g.workspace_id,g.target_kind,g.target_id,g.grantee_user_id,u.display_name,u.normalized_email AS email,g.role,g.created_by_user_id,g.created_at,g.revoked_at,g.version, CASE g.target_kind WHEN 'file' THEN f.name ELSE p.name END AS target_name FROM grants g JOIN users u ON u.id=g.grantee_user_id LEFT JOIN files f ON g.target_kind='file' AND f.org_id=g.org_id AND f.workspace_id=g.workspace_id AND f.id=g.target_id LEFT JOIN projects p ON g.target_kind='project' AND p.org_id=g.org_id AND p.workspace_id=g.workspace_id AND p.id=g.target_id WHERE g.org_id=$1 AND g.revoked_at IS NULL AND g.workspace_id=ANY($2) AND ($3::uuid IS NULL OR g.workspace_id=$3) AND ($4::uuid IS NULL OR g.grantee_user_id=$4) AND ($5::text IS NULL OR g.target_kind=$5) AND ($6::timestamptz IS NULL OR (g.created_at,g.id)>($6,$7)) ORDER BY g.created_at,g.id LIMIT $8"
    )
    .bind(scope.org_id()).bind(page.workspaces).bind(page.workspace_id).bind(page.user_id).bind(page.target_kind)
    .bind(page.after.map(|x|x.0)).bind(page.after.map(|x|x.1)).bind(crate::organizations::clamp_page(page.limit))
    .fetch_all(pool).await?)
}
pub async fn role_for(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    workspace_id: Uuid,
    file_id: Uuid,
    project_id: Uuid,
) -> Result<Option<GrantRole>, DbError> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM grants WHERE org_id=$1 AND workspace_id=$2 AND grantee_user_id=$3 AND revoked_at IS NULL AND ((target_kind='file' AND target_id=$4) OR (target_kind='project' AND target_id=$5)) ORDER BY CASE role WHEN 'editor' THEN 2 ELSE 1 END DESC LIMIT 1")
        .bind(org_id).bind(workspace_id).bind(user_id).bind(file_id).bind(project_id).fetch_optional(pool).await?;
    Ok(role.as_deref().and_then(GrantRole::parse))
}
pub async fn project_role_for(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    workspace_id: Uuid,
    project_id: Uuid,
) -> Result<Option<GrantRole>, DbError> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM grants WHERE org_id=$1 AND workspace_id=$2 AND grantee_user_id=$3 AND target_kind='project' AND target_id=$4 AND revoked_at IS NULL")
        .bind(org_id).bind(workspace_id).bind(user_id).bind(project_id).fetch_optional(pool).await?;
    Ok(role.as_deref().and_then(GrantRole::parse))
}
pub async fn shared_with_me(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    limit: i64,
    after: Option<(OffsetDateTime, Uuid)>,
) -> Result<Vec<Grant>, DbError> {
    Ok(sqlx::query_as::<_, Grant>(&format!("{SELECT} WHERE g.org_id=$1 AND g.grantee_user_id=$2 AND g.revoked_at IS NULL AND ($3::timestamptz IS NULL OR (g.created_at,g.id)>($3,$4)) ORDER BY g.created_at,g.id LIMIT $5"))
        .bind(org_id).bind(user_id).bind(after.map(|x| x.0)).bind(after.map(|x| x.1)).bind(limit.clamp(1, 101)).fetch_all(pool).await?)
}

pub async fn shared_with_me_items(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    limit: i64,
    after: Option<(OffsetDateTime, Uuid)>,
) -> Result<Vec<SharedWithMeItem>, DbError> {
    Ok(sqlx::query_as::<_, SharedWithMeItem>("SELECT g.created_at,g.id,g.target_kind,g.target_id,g.role,w.id AS workspace_id,w.name AS workspace_name,p.id AS project_id,p.name AS project_name,f.id AS file_id,f.name AS file_name FROM grants g JOIN workspaces w ON w.org_id=g.org_id AND w.id=g.workspace_id LEFT JOIN files f ON g.target_kind='file' AND f.org_id=g.org_id AND f.workspace_id=g.workspace_id AND f.id=g.target_id LEFT JOIN projects p ON p.org_id=g.org_id AND p.workspace_id=g.workspace_id AND p.id=CASE WHEN g.target_kind='project' THEN g.target_id ELSE f.project_id END WHERE g.org_id=$1 AND g.grantee_user_id=$2 AND g.revoked_at IS NULL AND ($3::timestamptz IS NULL OR (g.created_at,g.id)>($3,$4)) ORDER BY g.created_at,g.id LIMIT $5")
        .bind(org_id).bind(user_id).bind(after.map(|x|x.0)).bind(after.map(|x|x.1)).bind(limit.clamp(1,101)).fetch_all(pool).await?)
}

impl OrgTx {
    pub async fn policy_resource(
        &mut self,
        user_id: Uuid,
        workspace_id: Uuid,
        id: Uuid,
        action: Action,
    ) -> Result<PolicyResource, DbError> {
        use Action as A;

        let grant_action = matches!(action, A::GrantList | A::GrantCreate | A::GrantChange);
        let project_action = matches!(
            action,
            A::ProjectRead
                | A::FileList
                | A::FileCreate
                | A::ProjectRename
                | A::ProjectDelete
                | A::ProjectRestore
        );
        let file_action = matches!(
            action,
            A::FileRead
                | A::FileRename
                | A::FileDelete
                | A::FileRestore
                | A::UploadCreate
                | A::UploadWrite
                | A::VersionList
                | A::VersionRead
                | A::FileDownload
                | A::VersionCommit
                | A::VersionRestore
                | A::FileLockAcquire
                | A::FileLockRelease
                | A::FileLockForce
        );

        if grant_action || project_action {
            let is_project: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE org_id=$1 AND workspace_id=$2 AND id=$3)",
            )
            .bind(self.org_id)
            .bind(workspace_id)
            .bind(id)
            .fetch_one(&mut *self.tx)
            .await?;
            if is_project {
                let role: Option<String> = sqlx::query_scalar(
                    "SELECT role FROM grants WHERE org_id=$1 AND workspace_id=$2 AND grantee_user_id=$3 AND target_kind='project' AND target_id=$4 AND revoked_at IS NULL",
                )
                .bind(self.org_id)
                .bind(workspace_id)
                .bind(user_id)
                .bind(id)
                .fetch_optional(&mut *self.tx)
                .await?;
                return Ok(PolicyResource::Object {
                    org_id: self.org_id,
                    workspace_id,
                    target: ObjectTarget::Project {
                        grant_role: policy_role(role),
                    },
                });
            }
            if project_action {
                return Err(DbError::Internal);
            }
        }

        if file_action || grant_action {
            let project_id: Option<Uuid> = sqlx::query_scalar(
                "SELECT project_id FROM files WHERE org_id=$1 AND workspace_id=$2 AND id=$3",
            )
            .bind(self.org_id)
            .bind(workspace_id)
            .bind(id)
            .fetch_optional(&mut *self.tx)
            .await?;
            let project_id = project_id.ok_or(DbError::Internal)?;
            let role: Option<String> = sqlx::query_scalar(
                "SELECT role FROM grants WHERE org_id=$1 AND workspace_id=$2 AND grantee_user_id=$3 AND ((target_kind='file' AND target_id=$4) OR (target_kind='project' AND target_id=$5)) AND revoked_at IS NULL ORDER BY CASE role WHEN 'editor' THEN 2 ELSE 1 END DESC LIMIT 1",
            )
            .bind(self.org_id)
            .bind(workspace_id)
            .bind(user_id)
            .bind(id)
            .bind(project_id)
            .fetch_optional(&mut *self.tx)
            .await?;
            return Ok(PolicyResource::Object {
                org_id: self.org_id,
                workspace_id,
                target: ObjectTarget::File {
                    grant_role: policy_role(role),
                },
            });
        }

        Ok(PolicyResource::Workspace {
            org_id: self.org_id,
            workspace_id,
        })
    }

    pub async fn grant_list(
        &mut self,
        scope: &OrgScope,
        target: Target,
    ) -> Result<Vec<Grant>, DbError> {
        self.check(scope, Action::GrantList)?;
        Ok(sqlx::query_as::<_, Grant>(&format!(
            "{SELECT} WHERE g.org_id=$1 AND g.workspace_id=$2 AND g.target_kind=$3 AND g.target_id=$4 AND g.revoked_at IS NULL ORDER BY g.created_at,g.id"
        ))
        .bind(self.org_id)
        .bind(target.workspace_id)
        .bind(target.kind.as_str())
        .bind(target.id)
        .fetch_all(&mut *self.tx)
        .await?)
    }

    pub async fn grant_create(
        &mut self,
        scope: &OrgScope,
        new: NewGrant,
    ) -> Result<CreateOutcome, DbError> {
        self.check(scope, craftcloud_core::policy::Action::GrantCreate)?;
        // (project trashed, file trashed); `None` when the target does not exist.
        let state: Option<(bool, bool)> = match new.target.kind {
            TargetKind::Project => sqlx::query_as("SELECT p.deleted_at IS NOT NULL, false FROM projects p WHERE p.org_id=$1 AND p.workspace_id=$2 AND p.id=$3").bind(self.org_id).bind(new.target.workspace_id).bind(new.target.id).fetch_optional(&mut *self.tx).await?,
            TargetKind::File => sqlx::query_as("SELECT p.deleted_at IS NOT NULL, f.deleted_at IS NOT NULL FROM files f JOIN projects p ON p.org_id=f.org_id AND p.id=f.project_id WHERE f.org_id=$1 AND f.workspace_id=$2 AND f.id=$3").bind(self.org_id).bind(new.target.workspace_id).bind(new.target.id).fetch_optional(&mut *self.tx).await?,
        };
        let Some((project_deleted, file_deleted)) = state else {
            return Ok(CreateOutcome::TargetGone);
        };
        if project_deleted {
            return Ok(CreateOutcome::ProjectDeleted);
        }
        if file_deleted {
            return Ok(CreateOutcome::FileDeleted);
        }
        let member: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_members m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND u.disabled_at IS NULL)")
            .bind(self.org_id).bind(new.grantee_user_id).fetch_one(&mut *self.tx).await?;
        if !member {
            return Ok(CreateOutcome::NotMember);
        }
        let (project_id, file_id) = match new.target.kind {
            TargetKind::Project => (Some(new.target.id), None),
            TargetKind::File => (None, Some(new.target.id)),
        };
        let id = Uuid::new_v4();
        let inserted = sqlx::query("INSERT INTO grants(id,org_id,workspace_id,target_kind,target_id,project_id,file_id,grantee_user_id,role,created_by_user_id,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(id).bind(self.org_id).bind(new.target.workspace_id).bind(new.target.kind.as_str()).bind(new.target.id).bind(project_id).bind(file_id).bind(new.grantee_user_id).bind(new.role.as_str()).bind(new.created_by).bind(new.now).execute(&mut *self.tx).await;
        match inserted {
            Ok(_) => Ok(CreateOutcome::Created(Box::new(
                self.grant(id).await?.ok_or(DbError::Internal)?,
            ))),
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                Ok(CreateOutcome::AlreadyExists)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn grant(&mut self, id: Uuid) -> Result<Option<Grant>, DbError> {
        Ok(
            sqlx::query_as::<_, Grant>(&format!("{SELECT} WHERE g.org_id=$1 AND g.id=$2"))
                .bind(self.org_id)
                .bind(id)
                .fetch_optional(&mut *self.tx)
                .await?,
        )
    }

    pub async fn grant_change(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        role: GrantRole,
        expected_version: i64,
    ) -> Result<ChangeOutcome, DbError> {
        self.check(scope, craftcloud_core::policy::Action::GrantChange)?;
        let current = self.grant(id).await?;
        let Some(current) = current else {
            return Ok(ChangeOutcome::Gone);
        };
        if current.revoked_at.is_some() {
            return Ok(ChangeOutcome::Gone);
        }
        if current.version != expected_version {
            return Ok(ChangeOutcome::VersionMismatch(current.version));
        }
        if current.role == role.as_str() {
            return Ok(ChangeOutcome::Changed(Box::new(current)));
        }
        sqlx::query("UPDATE grants SET role=$3,version=version+1 WHERE org_id=$1 AND id=$2 AND version=$4 AND revoked_at IS NULL")
            .bind(self.org_id).bind(id).bind(role.as_str()).bind(expected_version).execute(&mut *self.tx).await?;
        if current.role == GrantRole::Editor.as_str() && role == GrantRole::Viewer {
            match current.target_kind.as_str() {
                "file" => {
                    sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND file_id=$2 AND holder_user_id=$3")
                        .bind(self.org_id).bind(current.target_id).bind(current.grantee_user_id).execute(&mut *self.tx).await?;
                }
                "project" => {
                    sqlx::query("DELETE FROM file_locks l USING files f WHERE l.file_id=f.id AND l.org_id=$1 AND f.project_id=$2 AND l.holder_user_id=$3")
                        .bind(self.org_id).bind(current.target_id).bind(current.grantee_user_id).execute(&mut *self.tx).await?;
                }
                _ => return Err(DbError::Internal),
            }
        }
        Ok(ChangeOutcome::Changed(Box::new(
            self.grant(id).await?.ok_or(DbError::Internal)?,
        )))
    }

    pub async fn grant_revoke(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<Option<(Grant, bool)>, DbError> {
        self.check(scope, craftcloud_core::policy::Action::GrantChange)?;
        let Some(current) = self.grant(id).await? else {
            return Ok(None);
        };
        let mut changed = false;
        if current.revoked_at.is_none() {
            changed = sqlx::query("UPDATE grants SET revoked_at=$3,version=version+1 WHERE org_id=$1 AND id=$2 AND revoked_at IS NULL")
                .bind(self.org_id).bind(id).bind(now).execute(&mut *self.tx).await?.rows_affected() == 1;
            match current.target_kind.as_str() {
                "file" => {
                    sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND file_id=$2 AND holder_user_id=$3").bind(self.org_id).bind(current.target_id).bind(current.grantee_user_id).execute(&mut *self.tx).await?;
                }
                "project" => {
                    sqlx::query("DELETE FROM file_locks l USING files f WHERE l.file_id=f.id AND l.org_id=$1 AND f.project_id=$2 AND l.holder_user_id=$3").bind(self.org_id).bind(current.target_id).bind(current.grantee_user_id).execute(&mut *self.tx).await?;
                }
                _ => return Err(DbError::Internal),
            }
        }
        Ok(self.grant(id).await?.map(|grant| (grant, changed)))
    }
}
