//! File metadata of a project (CC-F06). No content and no versions yet:
//! `size_bytes` is 0, `sha256` and `head_version_id` are `NULL` until the first
//! version is committed by a later feature.
//!
//! Same rules as [`crate::projects`]: scoped reads, changes in an [`OrgTx`]
//! with exactly the scope of their own action, timestamps from the caller,
//! names unique among non-deleted files of a project by `name_key`. The file
//! name is display metadata only and never part of any path.
//!
//! A project in the trash is read-only: its files can be read and listed but
//! not created, renamed, deleted or restored ([`FileCreateOutcome::ProjectDeleted`],
//! [`FileRenameOutcome::ProjectDeleted`], ...).

use std::time::Duration;

use craftcloud_core::policy::Action;
use craftcloud_core::{FileApp, FileFormat, FileName};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::members::OrgTx;
use crate::organizations::{AfterKey, clamp_page};
use crate::projects::retention_expired;
use crate::{DbError, OrgScope};

/// Stored metadata of a file.
#[derive(Debug, Clone, FromRow)]
pub struct File {
    /// Identifier (also the future storage key; never the name).
    pub id: Uuid,
    /// Owning organization.
    pub org_id: Uuid,
    /// Owning workspace.
    pub workspace_id: Uuid,
    /// Owning project.
    pub project_id: Uuid,
    /// Display name (untrusted metadata).
    pub name: String,
    /// Application the file belongs to.
    pub app: String,
    /// Format label.
    pub format: String,
    /// Size of the head version in bytes (0 until a version exists).
    pub size_bytes: i64,
    /// SHA-256 of the head version, `None` until a version exists.
    pub sha256: Option<Vec<u8>>,
    /// Head version, `None` until the first commit.
    pub head_version_id: Option<Uuid>,
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
    /// When the file was moved to the trash.
    pub deleted_at: Option<OffsetDateTime>,
    /// Who moved the file to the trash.
    pub deleted_by_user_id: Option<Uuid>,
}

const COLUMNS: &str = "id, org_id, workspace_id, project_id, name, app, format, size_bytes, sha256, \
     head_version_id, version, created_by_user_id, created_at, updated_by_user_id, updated_at, \
     deleted_at, deleted_by_user_id";

/// Where a file lives, resolved from its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// Organization of the file.
    pub org_id: Uuid,
    /// Workspace of the file.
    pub workspace_id: Uuid,
    /// Project of the file.
    pub project_id: Uuid,
}

/// Resolves organization, workspace and project of a file id; `None` if unknown.
///
/// # Errors
///
/// Database failures only.
pub async fn locate(pool: &PgPool, file_id: Uuid) -> Result<Option<Location>, DbError> {
    let row: Option<(Uuid, Uuid, Uuid)> =
        sqlx::query_as("SELECT org_id, workspace_id, project_id FROM files WHERE id = $1")
            .bind(file_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(org_id, workspace_id, project_id)| Location {
        org_id,
        workspace_id,
        project_id,
    }))
}

pub async fn app_for(
    pool: &PgPool,
    org_id: Uuid,
    file_id: Uuid,
) -> Result<Option<String>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT app FROM files WHERE org_id=$1 AND id=$2")
            .bind(org_id)
            .bind(file_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn project_name(
    pool: &PgPool,
    org_id: Uuid,
    workspace_id: Uuid,
    project_id: Uuid,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT name FROM projects WHERE org_id=$1 AND workspace_id=$2 AND id=$3",
    )
    .bind(org_id)
    .bind(workspace_id)
    .bind(project_id)
    .fetch_optional(pool)
    .await?)
}

/// Fetches one file of the scoped organization (also a trashed one).
///
/// # Errors
///
/// Database failures only.
pub async fn get(pool: &PgPool, scope: &OrgScope, id: Uuid) -> Result<Option<File>, DbError> {
    Ok(sqlx::query_as::<_, File>(&format!(
        "SELECT {COLUMNS} FROM files WHERE org_id = $1 AND id = $2"
    ))
    .bind(scope.org_id())
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Lists the files of one project of the scoped organization ordered by
/// `(created_at, id)`: the live ones, or with `deleted` only the trash.
///
/// # Errors
///
/// Database failures only.
pub async fn list(
    pool: &PgPool,
    scope: &OrgScope,
    project_id: Uuid,
    deleted: bool,
    limit: u32,
    after: Option<AfterKey>,
) -> Result<Vec<File>, DbError> {
    Ok(sqlx::query_as::<_, File>(&format!(
        "SELECT {COLUMNS} FROM files \
         WHERE org_id = $1 AND project_id = $2 AND (deleted_at IS NOT NULL) = $3 \
           AND ($4::timestamptz IS NULL OR (created_at, id) > ($4::timestamptz, $5::uuid)) \
         ORDER BY created_at, id LIMIT $6"
    ))
    .bind(scope.org_id())
    .bind(project_id)
    .bind(deleted)
    .bind(after.map(|(at, _)| at))
    .bind(after.map(|(_, id)| id))
    .bind(clamp_page(limit))
    .fetch_all(pool)
    .await?)
}

/// Input for [`OrgTx::file_create`].
#[derive(Debug, Clone)]
pub struct NewFile {
    /// Validated name.
    pub name: FileName,
    /// Validated app identifier.
    pub app: FileApp,
    /// Validated format identifier.
    pub format: FileFormat,
}

/// Result of [`OrgTx::file_create`].
#[derive(Debug, Clone)]
pub enum FileCreateOutcome {
    /// Created.
    Created(Box<File>),
    /// A live file with the same name exists in the project.
    NameTaken,
    /// The project is in the trash.
    ProjectDeleted,
    /// No such project in this organization.
    ProjectGone,
}

/// Result of [`OrgTx::file_rename`].
#[derive(Debug, Clone)]
pub enum FileRenameOutcome {
    /// Renamed.
    Updated(Box<File>),
    /// The stored version differs from the expected one.
    VersionMismatch {
        /// The current version.
        current_version: i64,
    },
    /// A different live file has the same name.
    NameTaken,
    /// The file is in the trash.
    Deleted,
    /// The project is in the trash.
    ProjectDeleted,
    /// No such file in this organization.
    Gone,
}

/// Result of [`OrgTx::file_delete`].
#[derive(Debug, Clone)]
pub enum FileDeleteOutcome {
    /// Moved to the trash.
    Deleted(Box<File>),
    /// It already was in the trash (nothing changed).
    Unchanged(Box<File>),
    /// The project is in the trash.
    ProjectDeleted,
    /// FigCraft Live must be stopped before trashing this file.
    FigcraftLive,
    /// No such file in this organization.
    Gone,
}

/// Result of [`OrgTx::file_restore`].
#[derive(Debug, Clone)]
pub enum FileRestoreOutcome {
    /// Restored.
    Restored(Box<File>),
    /// It was not in the trash (nothing changed).
    Unchanged(Box<File>),
    /// The retention period has passed.
    Expired,
    /// A live file took the name meanwhile.
    NameTaken,
    /// The project is in the trash.
    ProjectDeleted,
    /// No such file in this organization.
    Gone,
}

impl OrgTx {
    /// Whether the project is in the trash; `None` if it does not exist in
    /// this organization.
    async fn project_trashed(&mut self, project_id: Uuid) -> Result<Option<(bool, Uuid)>, DbError> {
        let row: Option<(bool, Uuid)> = sqlx::query_as(
            "SELECT deleted_at IS NOT NULL, workspace_id FROM projects \
             WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(project_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        Ok(row)
    }

    async fn file_row(&mut self, id: Uuid) -> Result<Option<File>, DbError> {
        Ok(sqlx::query_as::<_, File>(&format!(
            "SELECT {COLUMNS} FROM files WHERE id = $1 AND org_id = $2 FOR UPDATE"
        ))
        .bind(id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    async fn file_name_taken(
        &mut self,
        project_id: Uuid,
        key: &str,
        except: Option<Uuid>,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM files \
             WHERE project_id = $1 AND name_key = $2 AND deleted_at IS NULL \
               AND ($3::uuid IS NULL OR id <> $3))",
        )
        .bind(project_id)
        .bind(key)
        .bind(except)
        .fetch_one(&mut *self.tx)
        .await?)
    }

    /// Creates a file (metadata only) in a project of this organization.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn file_create(
        &mut self,
        scope: &OrgScope,
        project_id: Uuid,
        new: &NewFile,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<FileCreateOutcome, DbError> {
        self.check(scope, Action::FileCreate)?;
        let workspace_id = match self.project_trashed(project_id).await? {
            None => return Ok(FileCreateOutcome::ProjectGone),
            Some((true, _)) => return Ok(FileCreateOutcome::ProjectDeleted),
            Some((false, workspace_id)) => workspace_id,
        };
        let key = new.name.key();
        if self.file_name_taken(project_id, &key, None).await? {
            return Ok(FileCreateOutcome::NameTaken);
        }
        let created = sqlx::query_as::<_, File>(&format!(
            "INSERT INTO files (id, org_id, workspace_id, project_id, name, name_key, app, format, \
                 created_by_user_id, created_at, updated_by_user_id, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $9, $10) RETURNING {COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(self.org_id)
        .bind(workspace_id)
        .bind(project_id)
        .bind(new.name.as_str())
        .bind(&key)
        .bind(new.app.as_str())
        .bind(new.format.as_str())
        .bind(user_id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await;
        match created {
            Ok(file) => Ok(FileCreateOutcome::Created(Box::new(file))),
            Err(error) => match DbError::from(error) {
                e if e.constraint() == Some("files_live_name_idx") => {
                    Ok(FileCreateOutcome::NameTaken)
                }
                e => Err(e),
            },
        }
    }

    /// Renames a file if its version is still `expected_version`.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn file_rename(
        &mut self,
        scope: &OrgScope,
        file_id: Uuid,
        name: &FileName,
        expected_version: i64,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<FileRenameOutcome, DbError> {
        self.check(scope, Action::FileRename)?;
        let Some(file) = self.file_row(file_id).await? else {
            return Ok(FileRenameOutcome::Gone);
        };
        if let Some((true, _)) = self.project_trashed(file.project_id).await? {
            return Ok(FileRenameOutcome::ProjectDeleted);
        }
        if file.deleted_at.is_some() {
            return Ok(FileRenameOutcome::Deleted);
        }
        if file.version != expected_version {
            return Ok(FileRenameOutcome::VersionMismatch {
                current_version: file.version,
            });
        }
        let key = name.key();
        if self
            .file_name_taken(file.project_id, &key, Some(file_id))
            .await?
        {
            return Ok(FileRenameOutcome::NameTaken);
        }
        let updated = sqlx::query_as::<_, File>(&format!(
            "UPDATE files SET name = $3, name_key = $4, version = version + 1, \
                 updated_by_user_id = $5, updated_at = $6 \
             WHERE id = $1 AND org_id = $2 RETURNING {COLUMNS}"
        ))
        .bind(file_id)
        .bind(self.org_id)
        .bind(name.as_str())
        .bind(&key)
        .bind(user_id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await;
        match updated {
            Ok(updated) => Ok(FileRenameOutcome::Updated(Box::new(updated))),
            Err(error) => match DbError::from(error) {
                e if e.constraint() == Some("files_live_name_idx") => {
                    Ok(FileRenameOutcome::NameTaken)
                }
                e => Err(e),
            },
        }
    }

    /// Moves a file to the trash (idempotent).
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn file_delete(
        &mut self,
        scope: &OrgScope,
        file_id: Uuid,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<FileDeleteOutcome, DbError> {
        self.check(scope, Action::FileDelete)?;
        let Some(file) = self.file_row(file_id).await? else {
            return Ok(FileDeleteOutcome::Gone);
        };
        if let Some((true, _)) = self.project_trashed(file.project_id).await? {
            return Ok(FileDeleteOutcome::ProjectDeleted);
        }
        if file.deleted_at.is_some() {
            return Ok(FileDeleteOutcome::Unchanged(Box::new(file)));
        }
        let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM figcraft_live_files WHERE org_id=$1 AND file_id=$2 AND state IN ('publishing','live','unpublishing'))")
            .bind(self.org_id).bind(file_id).fetch_one(&mut *self.tx).await?;
        if live {
            return Ok(FileDeleteOutcome::FigcraftLive);
        }
        let updated = sqlx::query_as::<_, File>(&format!(
            "UPDATE files SET deleted_at = $3, deleted_by_user_id = $4, version = version + 1, \
                 updated_by_user_id = $4, updated_at = $3 \
             WHERE id = $1 AND org_id = $2 RETURNING {COLUMNS}"
        ))
        .bind(file_id)
        .bind(self.org_id)
        .bind(now)
        .bind(user_id)
        .fetch_one(&mut *self.tx)
        .await?;
        crate::figcraft::enqueue_file_revocation(&mut self.tx, file_id, "file_deleted", now)
            .await?;
        Ok(FileDeleteOutcome::Deleted(Box::new(updated)))
    }

    /// Restores a file from the trash if the retention period has not passed
    /// and its name is still free.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn file_restore(
        &mut self,
        scope: &OrgScope,
        file_id: Uuid,
        user_id: Uuid,
        now: OffsetDateTime,
        retention: Duration,
    ) -> Result<FileRestoreOutcome, DbError> {
        self.check(scope, Action::FileRestore)?;
        let Some(file) = self.file_row(file_id).await? else {
            return Ok(FileRestoreOutcome::Gone);
        };
        if let Some((true, _)) = self.project_trashed(file.project_id).await? {
            return Ok(FileRestoreOutcome::ProjectDeleted);
        }
        let Some(deleted_at) = file.deleted_at else {
            return Ok(FileRestoreOutcome::Unchanged(Box::new(file)));
        };
        if retention_expired(deleted_at, now, retention) {
            return Ok(FileRestoreOutcome::Expired);
        }
        // The stored key decides, not a recomputation of the name.
        let key: String =
            sqlx::query_scalar("SELECT name_key FROM files WHERE id = $1 AND org_id = $2")
                .bind(file_id)
                .bind(self.org_id)
                .fetch_one(&mut *self.tx)
                .await?;
        if self
            .file_name_taken(file.project_id, &key, Some(file_id))
            .await?
        {
            return Ok(FileRestoreOutcome::NameTaken);
        }
        let updated = sqlx::query_as::<_, File>(&format!(
            "UPDATE files SET deleted_at = NULL, deleted_by_user_id = NULL, \
                 version = version + 1, updated_by_user_id = $3, updated_at = $4 \
             WHERE id = $1 AND org_id = $2 RETURNING {COLUMNS}"
        ))
        .bind(file_id)
        .bind(self.org_id)
        .bind(user_id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await;
        match updated {
            Ok(updated) => Ok(FileRestoreOutcome::Restored(Box::new(updated))),
            Err(error) => match DbError::from(error) {
                e if e.constraint() == Some("files_live_name_idx") => {
                    Ok(FileRestoreOutcome::NameTaken)
                }
                e => Err(e),
            },
        }
    }
}
