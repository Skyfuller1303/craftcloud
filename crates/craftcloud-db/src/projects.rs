//! Projects of a workspace (CC-F06).
//!
//! Reads take an [`OrgScope`] and filter by its `org_id` in SQL. Changes run in
//! an [`OrgTx`] (organization row lock, fresh roles, policy decision made by the
//! caller) via the `project_*` methods; each requires the scope of exactly its
//! own action. Timestamps come from the caller (the injectable clock), never
//! from the database, so retention is testable.
//!
//! Names are unique among non-deleted projects of a workspace by their
//! `name_key` (NFC + case folded). A soft-deleted project frees its name;
//! restoring it into a name that was taken meanwhile is refused.
//!
//! The workspace and organization of a project request are resolved from the
//! project row ([`locate`]), never from client supplied ids.

use std::time::Duration;

use craftcloud_core::ProjectName;
use craftcloud_core::policy::Action;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::members::OrgTx;
use crate::organizations::{AfterKey, clamp_page};
use crate::{DbError, OrgScope};

/// A stored project.
#[derive(Debug, Clone, FromRow)]
pub struct Project {
    /// Identifier.
    pub id: Uuid,
    /// Owning organization.
    pub org_id: Uuid,
    /// Owning workspace.
    pub workspace_id: Uuid,
    /// Display name.
    pub name: String,
    /// Optimistic concurrency version, incremented on every change.
    pub version: i64,
    /// Creator.
    pub created_by_user_id: Uuid,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change by.
    pub updated_by_user_id: Uuid,
    /// Last change time.
    pub updated_at: OffsetDateTime,
    /// When the project was moved to the trash.
    pub deleted_at: Option<OffsetDateTime>,
    /// Who moved the project to the trash.
    pub deleted_by_user_id: Option<Uuid>,
}

pub(crate) const COLUMNS: &str = "id, org_id, workspace_id, name, version, created_by_user_id, \
     created_at, updated_by_user_id, updated_at, deleted_at, deleted_by_user_id";

/// Where a project lives, resolved from its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// Organization of the project.
    pub org_id: Uuid,
    /// Workspace of the project.
    pub workspace_id: Uuid,
}

/// Resolves organization and workspace of a project id; `None` if unknown.
///
/// # Errors
///
/// Database failures only.
pub async fn locate(pool: &PgPool, project_id: Uuid) -> Result<Option<Location>, DbError> {
    let row: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT org_id, workspace_id FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(org_id, workspace_id)| Location {
        org_id,
        workspace_id,
    }))
}

/// Fetches one project of the scoped organization (also a trashed one). A
/// project of another organization is indistinguishable from a missing one.
///
/// # Errors
///
/// Database failures only.
pub async fn get(pool: &PgPool, scope: &OrgScope, id: Uuid) -> Result<Option<Project>, DbError> {
    Ok(sqlx::query_as::<_, Project>(&format!(
        "SELECT {COLUMNS} FROM projects WHERE org_id = $1 AND id = $2"
    ))
    .bind(scope.org_id())
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Lists the projects of one workspace of the scoped organization ordered by
/// `(created_at, id)`: the live ones, or with `deleted` only the trash. The
/// keyset contains no personal data.
///
/// # Errors
///
/// Database failures only.
pub async fn list(
    pool: &PgPool,
    scope: &OrgScope,
    workspace_id: Uuid,
    deleted: bool,
    limit: u32,
    after: Option<AfterKey>,
) -> Result<Vec<Project>, DbError> {
    Ok(sqlx::query_as::<_, Project>(&format!(
        "SELECT {COLUMNS} FROM projects \
         WHERE org_id = $1 AND workspace_id = $2 AND (deleted_at IS NOT NULL) = $3 \
           AND ($4::timestamptz IS NULL OR (created_at, id) > ($4::timestamptz, $5::uuid)) \
         ORDER BY created_at, id LIMIT $6"
    ))
    .bind(scope.org_id())
    .bind(workspace_id)
    .bind(deleted)
    .bind(after.map(|(at, _)| at))
    .bind(after.map(|(_, id)| id))
    .bind(clamp_page(limit))
    .fetch_all(pool)
    .await?)
}

/// Result of [`OrgTx::project_create`].
#[derive(Debug, Clone)]
pub enum ProjectCreateOutcome {
    /// Created.
    Created(Project),
    /// A live project with the same name exists in the workspace.
    NameTaken,
    /// The workspace does not exist in this organization.
    WorkspaceGone,
}

/// Result of [`OrgTx::project_rename`].
#[derive(Debug, Clone)]
pub enum ProjectRenameOutcome {
    /// Renamed.
    Updated(Project),
    /// The stored version differs from the expected one.
    VersionMismatch {
        /// The current version.
        current_version: i64,
    },
    /// A different live project has the same name.
    NameTaken,
    /// The project is in the trash.
    Deleted,
    /// No such project in this organization.
    Gone,
}

/// Result of [`OrgTx::project_delete`].
#[derive(Debug, Clone)]
pub enum ProjectDeleteOutcome {
    /// Moved to the trash.
    Deleted(Project),
    /// It already was in the trash (nothing changed).
    Unchanged(Project),
    /// No such project in this organization.
    Gone,
}

/// Result of [`OrgTx::project_restore`].
#[derive(Debug, Clone)]
pub enum ProjectRestoreOutcome {
    /// Restored.
    Restored(Project),
    /// It was not in the trash (nothing changed).
    Unchanged(Project),
    /// The retention period has passed.
    Expired,
    /// A live project took the name meanwhile.
    NameTaken,
    /// No such project in this organization.
    Gone,
}

pub(crate) fn retention_expired(
    deleted_at: OffsetDateTime,
    now: OffsetDateTime,
    retention: Duration,
) -> bool {
    let limit = time::Duration::try_from(retention).unwrap_or(time::Duration::MAX);
    now - deleted_at > limit
}

impl OrgTx {
    async fn project_row(&mut self, id: Uuid) -> Result<Option<Project>, DbError> {
        Ok(sqlx::query_as::<_, Project>(&format!(
            "SELECT {COLUMNS} FROM projects WHERE id = $1 AND org_id = $2 FOR UPDATE"
        ))
        .bind(id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    async fn project_name_taken(
        &mut self,
        workspace_id: Uuid,
        key: &str,
        except: Option<Uuid>,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM projects \
             WHERE workspace_id = $1 AND name_key = $2 AND deleted_at IS NULL \
               AND ($3::uuid IS NULL OR id <> $3))",
        )
        .bind(workspace_id)
        .bind(key)
        .bind(except)
        .fetch_one(&mut *self.tx)
        .await?)
    }

    /// Creates a project in a workspace of this organization.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn project_create(
        &mut self,
        scope: &OrgScope,
        workspace_id: Uuid,
        name: &ProjectName,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<ProjectCreateOutcome, DbError> {
        self.check(scope, Action::ProjectCreate)?;
        if !self.workspace_exists(workspace_id).await? {
            return Ok(ProjectCreateOutcome::WorkspaceGone);
        }
        let key = name.key();
        if self.project_name_taken(workspace_id, &key, None).await? {
            return Ok(ProjectCreateOutcome::NameTaken);
        }
        let created = sqlx::query_as::<_, Project>(&format!(
            "INSERT INTO projects (id, org_id, workspace_id, name, name_key, \
                 created_by_user_id, created_at, updated_by_user_id, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $6, $7) RETURNING {COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(self.org_id)
        .bind(workspace_id)
        .bind(name.as_str())
        .bind(&key)
        .bind(user_id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await;
        match created {
            Ok(project) => Ok(ProjectCreateOutcome::Created(project)),
            Err(error) => match DbError::from(error) {
                e if e.constraint() == Some("projects_live_name_idx") => {
                    Ok(ProjectCreateOutcome::NameTaken)
                }
                e => Err(e),
            },
        }
    }

    /// Renames a project if its version is still `expected_version`.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn project_rename(
        &mut self,
        scope: &OrgScope,
        project_id: Uuid,
        name: &ProjectName,
        expected_version: i64,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<ProjectRenameOutcome, DbError> {
        self.check(scope, Action::ProjectRename)?;
        let Some(project) = self.project_row(project_id).await? else {
            return Ok(ProjectRenameOutcome::Gone);
        };
        if project.deleted_at.is_some() {
            return Ok(ProjectRenameOutcome::Deleted);
        }
        if project.version != expected_version {
            return Ok(ProjectRenameOutcome::VersionMismatch {
                current_version: project.version,
            });
        }
        let key = name.key();
        if self
            .project_name_taken(project.workspace_id, &key, Some(project_id))
            .await?
        {
            return Ok(ProjectRenameOutcome::NameTaken);
        }
        let updated = sqlx::query_as::<_, Project>(&format!(
            "UPDATE projects SET name = $3, name_key = $4, version = version + 1, \
                 updated_by_user_id = $5, updated_at = $6 \
             WHERE id = $1 AND org_id = $2 RETURNING {COLUMNS}"
        ))
        .bind(project_id)
        .bind(self.org_id)
        .bind(name.as_str())
        .bind(&key)
        .bind(user_id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await;
        match updated {
            Ok(updated) => Ok(ProjectRenameOutcome::Updated(updated)),
            Err(error) => match DbError::from(error) {
                e if e.constraint() == Some("projects_live_name_idx") => {
                    Ok(ProjectRenameOutcome::NameTaken)
                }
                e => Err(e),
            },
        }
    }

    /// Moves a project to the trash (idempotent).
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn project_delete(
        &mut self,
        scope: &OrgScope,
        project_id: Uuid,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<ProjectDeleteOutcome, DbError> {
        self.check(scope, Action::ProjectDelete)?;
        let Some(project) = self.project_row(project_id).await? else {
            return Ok(ProjectDeleteOutcome::Gone);
        };
        if project.deleted_at.is_some() {
            return Ok(ProjectDeleteOutcome::Unchanged(project));
        }
        let updated = sqlx::query_as::<_, Project>(&format!(
            "UPDATE projects SET deleted_at = $3, deleted_by_user_id = $4, version = version + 1, \
                 updated_by_user_id = $4, updated_at = $3 \
             WHERE id = $1 AND org_id = $2 RETURNING {COLUMNS}"
        ))
        .bind(project_id)
        .bind(self.org_id)
        .bind(now)
        .bind(user_id)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(ProjectDeleteOutcome::Deleted(updated))
    }

    /// Restores a project from the trash if the retention period has not
    /// passed and its name is still free.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn project_restore(
        &mut self,
        scope: &OrgScope,
        project_id: Uuid,
        user_id: Uuid,
        now: OffsetDateTime,
        retention: Duration,
    ) -> Result<ProjectRestoreOutcome, DbError> {
        self.check(scope, Action::ProjectRestore)?;
        let Some(project) = self.project_row(project_id).await? else {
            return Ok(ProjectRestoreOutcome::Gone);
        };
        let Some(deleted_at) = project.deleted_at else {
            return Ok(ProjectRestoreOutcome::Unchanged(project));
        };
        if retention_expired(deleted_at, now, retention) {
            return Ok(ProjectRestoreOutcome::Expired);
        }
        // The stored key decides, not a recomputation of the name.
        let key: String =
            sqlx::query_scalar("SELECT name_key FROM projects WHERE id = $1 AND org_id = $2")
                .bind(project_id)
                .bind(self.org_id)
                .fetch_one(&mut *self.tx)
                .await?;
        if self
            .project_name_taken(project.workspace_id, &key, Some(project_id))
            .await?
        {
            return Ok(ProjectRestoreOutcome::NameTaken);
        }
        let updated = sqlx::query_as::<_, Project>(&format!(
            "UPDATE projects SET deleted_at = NULL, deleted_by_user_id = NULL, \
                 version = version + 1, updated_by_user_id = $3, updated_at = $4 \
             WHERE id = $1 AND org_id = $2 RETURNING {COLUMNS}"
        ))
        .bind(project_id)
        .bind(self.org_id)
        .bind(user_id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await;
        match updated {
            Ok(updated) => Ok(ProjectRestoreOutcome::Restored(updated)),
            Err(error) => match DbError::from(error) {
                e if e.constraint() == Some("projects_live_name_idx") => {
                    Ok(ProjectRestoreOutcome::NameTaken)
                }
                e => Err(e),
            },
        }
    }
}
