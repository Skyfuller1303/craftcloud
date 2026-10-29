//! Immutable file versions, compare-and-set commits and restores (CC-F08).
//!
//! A version binds a file to a verified blob. The head of a file only moves
//! inside an [`OrgTx`] (organization lock, fresh roles, policy decision by the
//! caller) with a compare-and-set on the head the client based its work on:
//! `If-None-Match: *` for the first version, `If-Match: "version:<id>"` for
//! later ones. Of two writers on the same base exactly one wins; the other
//! gets [`CommitOutcome::PreconditionFailed`] and keeps its stored upload, which
//! can still become the first version of another file.
//!
//! An upload is committed to a file of its own workspace; that is normally the
//! file it was created for, but after a lost race the loser can commit the same
//! stored upload to a new file (a copy) instead of uploading again.
//!
//! Quota: a commit moves the upload's bytes from `reserved_bytes` to
//! `committed_bytes` (organization and workspace, exactly once, together with
//! the blob state). A restore references an existing blob and adds no bytes.
//! A quota lowered after the reservation does not cancel it.

use craftcloud_core::policy::Action;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::members::OrgTx;
use crate::{DbError, OrgScope};

/// One immutable version of a file.
#[derive(Debug, Clone, FromRow)]
pub struct FileVersion {
    /// Identifier.
    pub id: Uuid,
    /// Organization.
    pub org_id: Uuid,
    /// Workspace.
    pub workspace_id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// The file.
    pub file_id: Uuid,
    /// 1, 2, 3, ... per file.
    pub number: i64,
    /// The blob with the content.
    pub blob_id: Uuid,
    /// Size in bytes.
    pub size_bytes: i64,
    /// SHA-256 of the content.
    pub sha256: Vec<u8>,
    /// The head this version was committed on top of.
    pub base_version_id: Option<Uuid>,
    /// The version a restore copied, if this is a restore.
    pub restored_from_version_id: Option<Uuid>,
    /// The upload session that produced the content.
    pub upload_session_id: Option<Uuid>,
    /// Who committed it.
    pub created_by_user_id: Uuid,
    /// When.
    pub created_at: OffsetDateTime,
}

const COLUMNS: &str = "id, org_id, workspace_id, project_id, file_id, number, blob_id, size_bytes, \
     sha256, base_version_id, restored_from_version_id, upload_session_id, created_by_user_id, \
     created_at";

/// What a new version is made of.
struct NewVersion<'a> {
    /// Workspace, project and file.
    file: (Uuid, Uuid, Uuid),
    blob_id: Uuid,
    size: i64,
    sha256: &'a [u8],
    base: Option<Uuid>,
    restored_from: Option<Uuid>,
    upload_session: Option<Uuid>,
}

/// The client's precondition on the head of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition {
    /// `If-None-Match: *`: the file must not have a version yet.
    NoVersionYet,
    /// `If-Match: "version:<id>"`: the head must be this version. `None`
    /// stands for a value that is not a version tag and never matches.
    Head(Option<Uuid>),
}

impl Precondition {
    fn holds(self, head: Option<Uuid>) -> bool {
        match self {
            Self::NoVersionYet => head.is_none(),
            Self::Head(Some(expected)) => head == Some(expected),
            Self::Head(None) => false,
        }
    }
}

/// Result of [`OrgTx::version_commit`].
#[derive(Debug, Clone)]
pub enum CommitOutcome {
    /// A new version, head moved.
    Committed(Box<FileVersion>),
    /// The upload had already been committed: its version (idempotent retry).
    Replayed(Box<FileVersion>),
    /// The head is not what the client based its work on.
    PreconditionFailed {
        /// The current head, `None` if the file has no version.
        current: Option<Uuid>,
    },
    /// Neither `If-Match` nor `If-None-Match` was sent for a commit that has to happen.
    PreconditionMissing,
    /// No such file or upload (or the upload was committed to another file).
    Gone,
    /// The upload has not finished receiving (`created`, `receiving`).
    NotStored(String),
    /// The upload is closed without content (`aborted`, `failed`).
    Closed(String),
    /// The upload session expired.
    Expired,
    /// The file is in the trash.
    FileDeleted,
    /// The project is in the trash.
    ProjectDeleted,
}

/// Result of [`OrgTx::version_restore`].
#[derive(Debug, Clone)]
pub enum RestoreOutcome {
    /// A new version that references the old blob.
    Restored(Box<FileVersion>),
    /// The head is not what the client based its work on.
    PreconditionFailed {
        /// The current head, `None` if the file has no version.
        current: Option<Uuid>,
    },
    /// No such file or version.
    Gone,
    /// The file is in the trash.
    FileDeleted,
    /// The project is in the trash.
    ProjectDeleted,
    /// The blob of that version failed verification before.
    BlobCorrupt,
}

/// What a download needs, resolved under the caller's scope.
#[derive(Debug, Clone)]
pub enum DownloadTarget {
    /// No such file or version in this organization.
    Gone,
    /// The file is in the trash.
    FileDeleted,
    /// The project is in the trash.
    ProjectDeleted,
    /// The file has no version yet.
    NoVersion,
    /// Ready.
    Ready(Box<DownloadInfo>),
}

/// The content of one version.
#[derive(Debug, Clone)]
pub struct DownloadInfo {
    /// The version.
    pub version_id: Uuid,
    /// The blob.
    pub blob_id: Uuid,
    /// The storage key.
    pub object_key: String,
    /// Recorded size.
    pub size_bytes: i64,
    /// Recorded SHA-256.
    pub sha256: Vec<u8>,
    /// Blob state (`committed`, or `corrupt`).
    pub blob_state: String,
    /// Display name of the file (untrusted).
    pub file_name: String,
}

/// Fetches one version of a file in the scoped organization.
///
/// # Errors
///
/// Database failures only.
pub async fn get(
    pool: &PgPool,
    scope: &OrgScope,
    file_id: Uuid,
    version_id: Uuid,
) -> Result<Option<FileVersion>, DbError> {
    Ok(sqlx::query_as::<_, FileVersion>(&format!(
        "SELECT {COLUMNS} FROM file_versions WHERE org_id = $1 AND file_id = $2 AND id = $3"
    ))
    .bind(scope.org_id())
    .bind(file_id)
    .bind(version_id)
    .fetch_optional(pool)
    .await?)
}

/// Lists the versions of a file, newest first, keyset paginated by number.
///
/// # Errors
///
/// Database failures only.
pub async fn list(
    pool: &PgPool,
    scope: &OrgScope,
    file_id: Uuid,
    limit: u32,
    before_number: Option<i64>,
) -> Result<Vec<FileVersion>, DbError> {
    Ok(sqlx::query_as::<_, FileVersion>(&format!(
        "SELECT {COLUMNS} FROM file_versions \
         WHERE org_id = $1 AND file_id = $2 AND ($3::bigint IS NULL OR number < $3) \
         ORDER BY number DESC LIMIT $4"
    ))
    .bind(scope.org_id())
    .bind(file_id)
    .bind(before_number)
    .bind(i64::from(limit.clamp(1, 200)))
    .fetch_all(pool)
    .await?)
}

/// The current head of a file in the scoped organization (`None` = no such
/// file; inner `None` = no version yet).
///
/// # Errors
///
/// Database failures only.
pub async fn head_of(
    pool: &PgPool,
    scope: &OrgScope,
    file_id: Uuid,
) -> Result<Option<Option<Uuid>>, DbError> {
    let row: Option<(Option<Uuid>,)> =
        sqlx::query_as("SELECT head_version_id FROM files WHERE org_id = $1 AND id = $2")
            .bind(scope.org_id())
            .bind(file_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(head,)| head))
}

/// Resolves the content to download: the head, or the given version.
///
/// # Errors
///
/// Database failures only.
pub async fn download_target(
    pool: &PgPool,
    scope: &OrgScope,
    file_id: Uuid,
    version: Option<Uuid>,
) -> Result<DownloadTarget, DbError> {
    let file: Option<(Option<Uuid>, bool, bool, String)> = sqlx::query_as(
        "SELECT f.head_version_id, f.deleted_at IS NOT NULL, p.deleted_at IS NOT NULL, f.name \
         FROM files f JOIN projects p ON p.id = f.project_id \
         WHERE f.org_id = $1 AND f.id = $2",
    )
    .bind(scope.org_id())
    .bind(file_id)
    .fetch_optional(pool)
    .await?;
    let Some((head, file_deleted, project_deleted, file_name)) = file else {
        return Ok(DownloadTarget::Gone);
    };
    if project_deleted {
        return Ok(DownloadTarget::ProjectDeleted);
    }
    if file_deleted {
        return Ok(DownloadTarget::FileDeleted);
    }
    let wanted = match (version, head) {
        (Some(id), _) => id,
        (None, Some(head)) => head,
        (None, None) => return Ok(DownloadTarget::NoVersion),
    };
    let row: Option<(Uuid, Uuid, String, i64, Vec<u8>, String)> = sqlx::query_as(
        "SELECT v.id, b.id, b.object_key, v.size_bytes, v.sha256, b.state \
         FROM file_versions v JOIN blobs b ON b.id = v.blob_id AND b.org_id = v.org_id \
         WHERE v.org_id = $1 AND v.file_id = $2 AND v.id = $3",
    )
    .bind(scope.org_id())
    .bind(file_id)
    .bind(wanted)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((version_id, blob_id, object_key, size_bytes, sha256, blob_state)) => {
            DownloadTarget::Ready(Box::new(DownloadInfo {
                version_id,
                blob_id,
                object_key,
                size_bytes,
                sha256,
                blob_state,
                file_name,
            }))
        }
        None => DownloadTarget::Gone,
    })
}

/// Marks a blob whose bytes failed verification. The row stays (the data is
/// evidence), the state tells operators and later downloads.
///
/// # Errors
///
/// Database failures only.
pub async fn mark_blob_corrupt(pool: &PgPool, blob_id: Uuid) -> Result<(), DbError> {
    sqlx::query("UPDATE blobs SET state = 'corrupt' WHERE id = $1 AND state = 'committed'")
        .bind(blob_id)
        .execute(pool)
        .await?;
    Ok(())
}

impl OrgTx {
    /// Locks the file and returns `(head, file_deleted, project_deleted,
    /// workspace_id, project_id)`.
    async fn file_head_locked(
        &mut self,
        file_id: Uuid,
    ) -> Result<Option<(Option<Uuid>, bool, bool, Uuid, Uuid)>, DbError> {
        Ok(sqlx::query_as(
            "SELECT f.head_version_id, f.deleted_at IS NOT NULL, p.deleted_at IS NOT NULL, \
                    f.workspace_id, f.project_id \
             FROM files f JOIN projects p ON p.id = f.project_id \
             WHERE f.id = $1 AND f.org_id = $2 FOR UPDATE OF f",
        )
        .bind(file_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    async fn insert_version(
        &mut self,
        new: NewVersion<'_>,
        user_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<FileVersion, DbError> {
        let NewVersion {
            file: (workspace_id, project_id, file_id),
            blob_id,
            size,
            sha256,
            base,
            restored_from,
            upload_session,
        } = new;
        let number: i64 = sqlx::query_scalar(
            "SELECT COALESCE(max(number), 0) + 1 FROM file_versions WHERE file_id = $1",
        )
        .bind(file_id)
        .fetch_one(&mut *self.tx)
        .await?;
        let version = sqlx::query_as::<_, FileVersion>(&format!(
            "INSERT INTO file_versions (id, org_id, workspace_id, project_id, file_id, number, \
                 blob_id, size_bytes, sha256, base_version_id, restored_from_version_id, \
                 upload_session_id, created_by_user_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
             RETURNING {COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(self.org_id)
        .bind(workspace_id)
        .bind(project_id)
        .bind(file_id)
        .bind(number)
        .bind(blob_id)
        .bind(size)
        .bind(sha256)
        .bind(base)
        .bind(restored_from)
        .bind(upload_session)
        .bind(user_id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await?;
        // Compare-and-set of the head (the file row is locked, so this is the
        // check the lock makes race free; it also guards against mistakes).
        let moved = sqlx::query(
            "UPDATE files SET head_version_id = $2, size_bytes = $3, sha256 = $4, \
                 version = version + 1, updated_by_user_id = $5, updated_at = $6 \
             WHERE id = $1 AND org_id = $7 AND head_version_id IS NOT DISTINCT FROM $8",
        )
        .bind(file_id)
        .bind(version.id)
        .bind(size)
        .bind(sha256)
        .bind(user_id)
        .bind(now)
        .bind(self.org_id)
        .bind(base)
        .execute(&mut *self.tx)
        .await?;
        if moved.rows_affected() != 1 {
            return Err(DbError::Internal);
        }
        Ok(version)
    }

    /// Commits a stored upload as the next version of a file.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn version_commit(
        &mut self,
        scope: &OrgScope,
        file_id: Uuid,
        upload_id: Uuid,
        user_id: Uuid,
        precondition: Option<Precondition>,
        now: OffsetDateTime,
    ) -> Result<CommitOutcome, DbError> {
        self.check(scope, Action::VersionCommit)?;
        let Some((head, file_deleted, project_deleted, workspace_id, project_id)) =
            self.file_head_locked(file_id).await?
        else {
            return Ok(CommitOutcome::Gone);
        };
        let Some(session) = self.session_locked(upload_id, now).await? else {
            return Ok(CommitOutcome::Gone);
        };
        // Defense in depth: the upload must live in the workspace of the locked file.
        if session.workspace_id != workspace_id {
            return Ok(CommitOutcome::Gone);
        }
        // A retry of a finished commit returns the version, whatever the
        // preconditions say now.
        if session.state == "committed" {
            let existing = sqlx::query_as::<_, FileVersion>(&format!(
                "SELECT {COLUMNS} FROM file_versions WHERE upload_session_id = $1"
            ))
            .bind(upload_id)
            .fetch_optional(&mut *self.tx)
            .await?;
            return Ok(match existing {
                Some(version) if version.file_id == file_id => {
                    CommitOutcome::Replayed(Box::new(version))
                }
                _ => CommitOutcome::Gone,
            });
        }
        match session.state.as_str() {
            "stored" => {}
            "expired" => return Ok(CommitOutcome::Expired),
            "created" | "receiving" => return Ok(CommitOutcome::NotStored(session.state)),
            _ => return Ok(CommitOutcome::Closed(session.state)),
        }
        if project_deleted {
            return Ok(CommitOutcome::ProjectDeleted);
        }
        if file_deleted {
            return Ok(CommitOutcome::FileDeleted);
        }
        // A missing precondition is only an error for a commit that still has to happen
        // (a retry of a finished commit never gets here).
        let Some(precondition) = precondition else {
            return Ok(CommitOutcome::PreconditionMissing);
        };
        if !precondition.holds(head) {
            return Ok(CommitOutcome::PreconditionFailed { current: head });
        }
        let (Some(blob_id), size) = (session.blob_id, session.declared_size) else {
            return Ok(CommitOutcome::Closed(session.state));
        };
        let version = self
            .insert_version(
                NewVersion {
                    file: (workspace_id, project_id, file_id),
                    blob_id,
                    size,
                    sha256: &session.declared_sha256,
                    base: head,
                    restored_from: None,
                    upload_session: Some(upload_id),
                },
                user_id,
                now,
            )
            .await?;
        sqlx::query("UPDATE blobs SET state = 'committed' WHERE id = $1 AND org_id = $2")
            .bind(blob_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?;
        sqlx::query(
            "UPDATE upload_sessions SET state = 'committed', updated_at = $3, version = version + 1 \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(upload_id)
        .bind(self.org_id)
        .bind(now)
        .execute(&mut *self.tx)
        .await?;
        // reserved -> committed, once, for workspace and organization.
        self.release(workspace_id, size).await?;
        self.add_committed(workspace_id, size).await?;
        Ok(CommitOutcome::Committed(Box::new(version)))
    }

    /// Creates a new version that references the blob of an older one.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn version_restore(
        &mut self,
        scope: &OrgScope,
        file_id: Uuid,
        version_id: Uuid,
        user_id: Uuid,
        precondition: Precondition,
        now: OffsetDateTime,
    ) -> Result<RestoreOutcome, DbError> {
        self.check(scope, Action::VersionRestore)?;
        let Some((head, file_deleted, project_deleted, workspace_id, project_id)) =
            self.file_head_locked(file_id).await?
        else {
            return Ok(RestoreOutcome::Gone);
        };
        if project_deleted {
            return Ok(RestoreOutcome::ProjectDeleted);
        }
        if file_deleted {
            return Ok(RestoreOutcome::FileDeleted);
        }
        let old: Option<(Uuid, i64, Vec<u8>, String)> = sqlx::query_as(
            "SELECT v.blob_id, v.size_bytes, v.sha256, b.state FROM file_versions v \
             JOIN blobs b ON b.id = v.blob_id AND b.org_id = v.org_id \
             WHERE v.org_id = $1 AND v.file_id = $2 AND v.id = $3",
        )
        .bind(self.org_id)
        .bind(file_id)
        .bind(version_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((blob_id, size, sha256, blob_state)) = old else {
            return Ok(RestoreOutcome::Gone);
        };
        if !precondition.holds(head) {
            return Ok(RestoreOutcome::PreconditionFailed { current: head });
        }
        if blob_state != "committed" {
            return Ok(RestoreOutcome::BlobCorrupt);
        }
        let version = self
            .insert_version(
                NewVersion {
                    file: (workspace_id, project_id, file_id),
                    blob_id,
                    size,
                    sha256: &sha256,
                    base: head,
                    restored_from: Some(version_id),
                    upload_session: None,
                },
                user_id,
                now,
            )
            .await?;
        Ok(RestoreOutcome::Restored(Box::new(version)))
    }
}
