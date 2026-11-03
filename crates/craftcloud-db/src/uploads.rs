//! Upload sessions, staged blobs and storage quota accounting (CC-F07, CC-F10).
//!
//! An upload session reserves quota, receives one stream into object storage
//! and ends as a verified but uncommitted ("staged") blob; the version commit
//! comes later. All changes run in an [`OrgTx`], so they are serialized per
//! organization together with the usage counters:
//!
//! - `organizations.reserved_bytes` / `workspaces.reserved_bytes` hold the
//!   declared size of every session in the states `created`, `receiving` and
//!   `stored` (a stored blob keeps its reservation until a version commit moves
//!   it to `committed_bytes`), plus the size of every `orphaned` blob;
//! - a session that is aborted, fails or expires releases its reservation if
//!   it has no object yet. A `stored` session already has a published object:
//!   its staged blob becomes `orphaned` and the bytes keep counting in
//!   `reserved_bytes` until the cleanup job has claimed the blob for deletion
//!   ([`OrgTx::maint_gc_claim`]), so aborting cannot be used to store more than
//!   the quota allows;
//! - the counters can be recomputed from the rows at any time
//!   ([`OrgTx::recompute_usage`]).
//!
//! Lock order: organization row (by [`OrgTx::begin`]) always first. Creation
//! then locks the creating user's row (per-user cap) and the workspace row; the
//! lazy expiry that runs before it may already have updated workspace rows of
//! the same organization. Both orders are safe because every workspace row is
//! only written while its organization row is held, and no transaction that
//! starts with a user row takes an organization or workspace lock afterwards.
//! Timestamps come from the caller (the injectable clock).

use craftcloud_core::policy::Action;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::members::OrgTx;
use crate::organizations::{AfterKey, clamp_page};
use crate::{DbError, OrgScope};

/// Session states that still hold a quota reservation.
pub(crate) const HOLDING: &str = "('created', 'receiving', 'stored')";
const MAX_MANIFEST_CHUNK_BYTES: i64 = 4 * 1024 * 1024;
type ExistingChunkRow = (Uuid, i64, Vec<u8>, String, String, String, String, i64);

/// A stored upload session.
#[derive(Debug, Clone, FromRow)]
pub struct UploadSession {
    /// Identifier.
    pub id: Uuid,
    /// Organization.
    pub org_id: Uuid,
    /// Workspace.
    pub workspace_id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// The file the upload is for.
    pub file_id: Uuid,
    /// Who created the session.
    pub created_by_user_id: Uuid,
    /// Declared size in bytes.
    pub declared_size: i64,
    /// Declared SHA-256.
    pub declared_sha256: Vec<u8>,
    /// The `Idempotency-Key` of the creating request, if any.
    pub idempotency_key: Option<String>,
    /// File, size and hash of the creating request.
    pub request_fingerprint: String,
    /// `created`, `receiving`, `stored`, `committed`, `aborted`, `expired` or `failed`.
    pub state: String,
    /// The staged blob once the content is stored.
    pub blob_id: Option<Uuid>,
    /// When the session expires.
    pub expires_at: OffsetDateTime,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
    /// Optimistic concurrency version.
    pub version: i64,
}

pub(crate) const SESSION_COLUMNS: &str = "id, org_id, workspace_id, project_id, file_id, created_by_user_id, \
     declared_size, declared_sha256, idempotency_key, request_fingerprint, state, blob_id, \
     expires_at, created_at, updated_at, version";

/// A blob in object storage that is (not yet) part of a file version.
#[derive(Debug, Clone, FromRow)]
pub struct Blob {
    /// Identifier.
    pub id: Uuid,
    /// Organization.
    pub org_id: Uuid,
    /// Workspace.
    pub workspace_id: Uuid,
    /// Logical quota group. Restore-created physical copies may share it.
    pub quota_group_id: Uuid,
    /// Storage key (32 lowercase hex characters).
    pub object_key: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// SHA-256.
    pub sha256: Vec<u8>,
    /// `staged`, `committed` or `orphaned`.
    pub state: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// One new physical chunk candidate for a staged file upload.
#[derive(Debug, Clone)]
pub struct NewChunk {
    /// File-scoped keyed content digest.
    pub digest: [u8; 32],
    /// Search digests from prior retained server keys, current first.
    pub digest_candidates: Vec<[u8; 32]>,
    /// Plain SHA-256 for storage integrity checks.
    pub sha256: [u8; 32],
    /// Opaque key of the already-published candidate object.
    pub object_key: String,
    /// Chunk size.
    pub size_bytes: i64,
    /// Stored byte length after optional compression.
    pub stored_bytes: i64,
    /// Physical storage encoding.
    pub encoding: String,
    /// Byte offset in the whole file.
    pub byte_offset: i64,
}

/// An existing local object selected for reuse after a newly uploaded
/// candidate was staged. The API verifies it before deleting the candidate.
#[derive(Debug, Clone)]
pub struct ChunkReuseCandidate {
    /// Stable chunk row id.
    pub chunk_id: Uuid,
    /// Existing key selected by the database.
    pub existing_object_key: String,
    /// Verified candidate key from this upload.
    pub candidate_object_key: String,
    /// Expected plaintext SHA-256.
    pub sha256: [u8; 32],
    /// Expected object size.
    pub size_bytes: i64,
    /// Stored byte length.
    pub stored_bytes: i64,
    /// Existing object's encoding.
    pub encoding: String,
    /// Candidate's encoding.
    pub candidate_encoding: String,
    /// Candidate's stored byte length.
    pub candidate_stored_bytes: i64,
}

/// One validated ordinal in a client-provided chunk manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkManifestSpec {
    /// Zero-based ordinal.
    pub ordinal: i32,
    /// Offset in the assembled file.
    pub byte_offset: i64,
    /// Plaintext byte length.
    pub size_bytes: i64,
    /// Plaintext SHA-256.
    pub sha256: [u8; 32],
    /// File-scoped HMAC identifier chosen at session creation.
    pub digest: [u8; 32],
}

/// One transient manifest row joined with its optional pinned chunk.
#[derive(Debug, Clone, FromRow)]
pub struct ChunkManifestRow {
    /// Zero-based ordinal.
    pub ordinal: i32,
    /// Offset in the assembled file.
    pub byte_offset: i64,
    /// Plaintext byte length.
    pub size_bytes: i64,
    /// Plaintext SHA-256.
    pub sha256: Vec<u8>,
    /// HMAC identifier selected at session creation.
    pub digest: Vec<u8>,
    /// Pinned physical chunk, absent while the ordinal is missing.
    pub chunk_id: Option<Uuid>,
    /// Local object key of the pinned chunk.
    pub object_key: Option<String>,
    /// Chunk tier.
    pub storage_tier: Option<String>,
    /// Stored byte length.
    pub stored_bytes: Option<i64>,
    /// Stored encoding.
    pub encoding: Option<String>,
}

/// A same-file candidate selected while the organization transaction is held.
#[derive(Debug, Clone, FromRow)]
pub struct ManifestChunkCandidate {
    /// Chunk row id.
    pub id: Uuid,
    /// HMAC identifier that matched.
    pub digest: Vec<u8>,
    /// Plaintext SHA-256.
    pub sha256: Vec<u8>,
    /// Object size.
    pub size_bytes: i64,
    /// Local object key.
    pub object_key: String,
    /// Chunk state.
    pub state: String,
    /// Storage tier.
    pub storage_tier: String,
    /// Stored byte length.
    pub stored_bytes: i64,
    /// Stored encoding.
    pub encoding: String,
}

/// Ordered manifest snapshot for verification outside a write transaction.
#[derive(Debug, Clone, FromRow)]
pub struct ChunkManifestSnapshotRow {
    /// Ordinal.
    pub ordinal: i32,
    /// Offset in the assembled file.
    pub byte_offset: i64,
    /// Expected plaintext length.
    pub size_bytes: i64,
    /// Expected plaintext SHA-256.
    pub sha256: Vec<u8>,
    /// Pinned chunk row.
    pub chunk_id: Option<Uuid>,
    /// Pinned local object key.
    pub object_key: Option<String>,
}

/// Quota and usage of an organization or workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromRow)]
pub struct Usage {
    /// Quota in bytes, `None` for unlimited.
    pub quota_bytes: Option<i64>,
    /// Bytes reserved by open uploads and staged blobs.
    pub reserved_bytes: i64,
    /// Bytes of committed versions.
    pub committed_bytes: i64,
}

/// Usage of one workspace (for the usage listing).
#[derive(Debug, Clone, FromRow)]
pub struct WorkspaceUsage {
    /// Workspace id.
    pub id: Uuid,
    /// Creation time of the workspace (keyset).
    pub created_at: OffsetDateTime,
    /// Quota in bytes, `None` for unlimited.
    pub quota_bytes: Option<i64>,
    /// Reserved bytes.
    pub reserved_bytes: i64,
    /// Committed bytes.
    pub committed_bytes: i64,
}

/// Accounted physical object bytes of one workspace.
#[derive(Debug, Clone, Copy, FromRow)]
pub struct PhysicalWorkspaceUsage {
    /// Workspace id.
    pub workspace_id: Uuid,
    /// Bytes currently represented by whole blobs and chunk objects.
    pub physical_bytes: i64,
}

/// Physical bytes by workspace for one organization.
///
/// Logical quota counters remain independent from this storage detail.
pub async fn physical_usage(
    pool: &PgPool,
    scope: &OrgScope,
) -> Result<(i64, Vec<PhysicalWorkspaceUsage>), DbError> {
    let rows = sqlx::query_as::<_, PhysicalWorkspaceUsage>(
        "SELECT workspace_id, SUM(stored_bytes)::bigint AS physical_bytes FROM ( \
           SELECT workspace_id, size_bytes AS stored_bytes FROM blobs WHERE org_id = $1 AND layout = 'whole' \
           UNION ALL \
           SELECT workspace_id, stored_bytes FROM chunks WHERE org_id = $1 \
           UNION ALL \
           SELECT r.workspace_id, r.physical_bytes FROM retired_archive_objects r WHERE r.org_id=$1 \
             AND NOT EXISTS (SELECT 1 FROM blobs b WHERE b.archive_object_key=r.archive_object_key) \
             AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.archive_object_key=r.archive_object_key) \
         ) physical GROUP BY workspace_id ORDER BY workspace_id",
    )
    .bind(scope.org_id())
    .fetch_all(pool)
    .await?;
    let total = rows.iter().try_fold(0i64, |sum, row| {
        sum.checked_add(row.physical_bytes).ok_or(DbError::Internal)
    })?;
    Ok((total, rows))
}

/// Where a session lives, resolved from its row.
#[derive(Debug, Clone, Copy)]
pub struct SessionLocation {
    /// Organization.
    pub org_id: Uuid,
    /// Workspace.
    pub workspace_id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// File.
    pub file_id: Uuid,
    /// Who created it.
    pub creator: Uuid,
}

/// Resolves organization, workspace, project, file and creator of an upload id.
///
/// # Errors
///
/// Database failures only.
pub async fn locate(pool: &PgPool, id: Uuid) -> Result<Option<SessionLocation>, DbError> {
    let row: Option<(Uuid, Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT org_id, workspace_id, project_id, file_id, created_by_user_id FROM upload_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(org_id, workspace_id, project_id, file_id, creator)| SessionLocation {
            org_id,
            workspace_id,
            project_id,
            file_id,
            creator,
        },
    ))
}
/// Reads one session of the scoped organization.
///
/// # Errors
///
/// Database failures only.
pub async fn get(
    pool: &PgPool,
    scope: &OrgScope,
    id: Uuid,
) -> Result<Option<UploadSession>, DbError> {
    Ok(sqlx::query_as::<_, UploadSession>(&format!(
        "SELECT {SESSION_COLUMNS} FROM upload_sessions WHERE org_id = $1 AND id = $2"
    ))
    .bind(scope.org_id())
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Reads a temporary manifest and its pinned objects for commit verification.
/// `None` identifies a legacy whole-object upload; `Some(empty)` is a valid
/// chunked upload of an empty file.
pub async fn chunk_manifest_snapshot(
    pool: &PgPool,
    scope: &OrgScope,
    id: Uuid,
) -> Result<Option<Vec<ChunkManifestSnapshotRow>>, DbError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM upload_chunk_manifests WHERE upload_id=$1 AND file_id=(SELECT file_id FROM upload_sessions WHERE id=$1 AND org_id=$2))",
    )
    .bind(id)
    .bind(scope.org_id())
    .fetch_one(pool)
    .await?;
    if !exists {
        return Ok(None);
    }
    let rows = sqlx::query_as::<_, ChunkManifestSnapshotRow>(
        "SELECT m.ordinal, m.byte_offset, m.size_bytes, m.sha256, u.chunk_id, c.object_key \
         FROM upload_chunk_manifest_entries m \
         LEFT JOIN upload_chunks u ON u.upload_id=m.upload_id AND u.ordinal=m.ordinal \
         LEFT JOIN chunks c ON c.id=u.chunk_id AND c.file_id=(SELECT file_id FROM upload_chunk_manifests WHERE upload_id=m.upload_id) \
         WHERE m.upload_id=$1 ORDER BY m.ordinal",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(Some(rows))
}

/// Missing ordinals for an authorized upload session. Empty list also covers
/// the valid empty-file chunk manifest.
pub async fn chunk_manifest_missing(
    pool: &PgPool,
    scope: &OrgScope,
    id: Uuid,
) -> Result<Option<Vec<u32>>, DbError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM upload_chunk_manifests WHERE upload_id=$1 AND file_id=(SELECT file_id FROM upload_sessions WHERE id=$1 AND org_id=$2))",
    )
    .bind(id)
    .bind(scope.org_id())
    .fetch_one(pool)
    .await?;
    if !exists {
        return Ok(None);
    }
    let rows: Vec<i32> = sqlx::query_scalar(
        "SELECT m.ordinal FROM upload_chunk_manifest_entries m \
         LEFT JOIN upload_chunks u ON u.upload_id=m.upload_id AND u.ordinal=m.ordinal \
         WHERE m.upload_id=$1 AND u.chunk_id IS NULL ORDER BY m.ordinal",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|ordinal| u32::try_from(ordinal).map_err(|_| DbError::Internal))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Quota and usage of the scoped organization.
///
/// # Errors
///
/// Database failures only.
pub async fn org_usage(pool: &PgPool, scope: &OrgScope) -> Result<Option<Usage>, DbError> {
    Ok(sqlx::query_as::<_, Usage>(
        "SELECT quota_bytes, reserved_bytes, committed_bytes FROM organizations WHERE id = $1",
    )
    .bind(scope.org_id())
    .fetch_optional(pool)
    .await?)
}

/// Usage of the workspaces of the scoped organization, keyset paginated by
/// `(created_at, id)`.
///
/// # Errors
///
/// Database failures only.
pub async fn workspace_usage(
    pool: &PgPool,
    scope: &OrgScope,
    limit: u32,
    after: Option<AfterKey>,
) -> Result<Vec<WorkspaceUsage>, DbError> {
    Ok(sqlx::query_as::<_, WorkspaceUsage>(
        "SELECT id, created_at, quota_bytes, reserved_bytes, committed_bytes FROM workspaces \
         WHERE org_id = $1 \
           AND ($2::timestamptz IS NULL OR (created_at, id) > ($2::timestamptz, $3::uuid)) \
         ORDER BY created_at, id LIMIT $4",
    )
    .bind(scope.org_id())
    .bind(after.map(|(at, _)| at))
    .bind(after.map(|(_, id)| id))
    .bind(clamp_page(limit))
    .fetch_all(pool)
    .await?)
}

/// Input for [`OrgTx::upload_create`].
#[derive(Debug, Clone)]
pub struct NewUpload {
    /// The file.
    pub file_id: Uuid,
    /// The creator.
    pub user_id: Uuid,
    /// Declared size in bytes.
    pub size: i64,
    /// Declared SHA-256.
    pub sha256: [u8; 32],
    /// `Idempotency-Key` header, if any.
    pub idempotency_key: Option<String>,
    /// Current time.
    pub now: OffsetDateTime,
    /// Expiry of the session.
    pub expires_at: OffsetDateTime,
    /// Cap of open uploads per user.
    pub max_per_user: i64,
    /// Cap of open uploads per organization.
    pub max_per_org: i64,
}

/// Result of [`OrgTx::upload_create`].
#[derive(Debug, Clone)]
pub enum UploadCreateOutcome {
    /// A new session with its reservation.
    Created(Box<UploadSession>),
    /// The same `Idempotency-Key` and request: the earlier session.
    Replayed(Box<UploadSession>),
    /// The `Idempotency-Key` was used with a different request.
    KeyReused,
    /// No such file in this organization.
    FileGone,
    /// The file is in the trash.
    FileDeleted,
    /// The project is in the trash.
    ProjectDeleted,
    /// The user has too many open uploads.
    UserLimit,
    /// The organization has too many open uploads.
    OrgLimit,
    /// The organization or workspace quota would be exceeded.
    QuotaExceeded,
}

/// Result of [`OrgTx::upload_begin_receiving`].
#[derive(Debug, Clone)]
pub enum BeginOutcome {
    /// The session is now `receiving`.
    Started(Box<UploadSession>),
    /// Another request is receiving the content.
    InProgress,
    /// The session expired (now or earlier).
    Expired,
    /// The session is closed (`stored`, `committed`, `aborted`, `failed`).
    Closed(String),
    /// The file is in the trash.
    FileDeleted,
    /// The project is in the trash.
    ProjectDeleted,
    /// No such session in this organization.
    Gone,
}

/// Result of [`OrgTx::upload_store`].
#[derive(Debug, Clone)]
pub enum StoreOutcome {
    /// The blob is recorded, the session is `stored`.
    Stored(Box<UploadSession>, Box<Blob>),
    /// The session is no longer `receiving` (expired, aborted by expiry, ...).
    NotReceiving(String),
    /// The file is in the trash.
    FileDeleted,
    /// The project is in the trash.
    ProjectDeleted,
    /// No such session in this organization.
    Gone,
}

/// Result of [`OrgTx::upload_abort`].
#[derive(Debug, Clone)]
pub enum AbortOutcome {
    /// Aborted; the reservation is released unless an object was published.
    Aborted(Box<UploadSession>),
    /// The session was already closed without a commit (nothing changed).
    Unchanged(Box<UploadSession>),
    /// Content is being received right now.
    InProgress,
    /// The upload was committed.
    Committed,
    /// No such session in this organization.
    Gone,
}

/// Result of setting a workspace quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSetOutcome {
    /// Stored (the value changed).
    Set,
    /// The workspace already had this quota; nothing changed.
    Unchanged,
    /// The workspace quota would exceed the organization quota.
    ExceedsOrganization,
    /// No such workspace in this organization.
    Gone,
}

fn fingerprint(file_id: Uuid, size: i64, sha256: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut text = format!("{file_id}:{size}:");
    for byte in sha256 {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

impl OrgTx {
    /// Checks the live-write barrier on the already-held organization connection.
    pub async fn figcraft_blocks_write(&mut self, file_id: Uuid) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM figcraft_live_files WHERE org_id=$1 AND file_id=$2 AND state IN ('publishing','live','unpublishing'))")
            .bind(self.org_id)
            .bind(file_id)
            .fetch_one(&mut *self.tx)
            .await?)
    }

    /// Checks whether any project file is in a live transition on this transaction.
    pub async fn figcraft_blocks_project_write(
        &mut self,
        project_id: Uuid,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM figcraft_live_files l JOIN files f ON f.id=l.file_id WHERE l.org_id=$1 AND f.project_id=$2 AND l.state IN ('publishing','live','unpublishing'))")
            .bind(self.org_id)
            .bind(project_id)
            .fetch_one(&mut *self.tx)
            .await?)
    }

    pub async fn upload_get(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        action: Action,
    ) -> Result<Option<UploadSession>, DbError> {
        self.check(scope, action)?;
        Ok(sqlx::query_as::<_, UploadSession>(&format!(
            "SELECT {SESSION_COLUMNS} FROM upload_sessions WHERE org_id=$1 AND id=$2"
        ))
        .bind(self.org_id)
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    /// Creates the temporary manifest for a newly reserved chunked upload.
    pub async fn upload_manifest_create(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        entries: &[ChunkManifestSpec],
        now: OffsetDateTime,
    ) -> Result<(), DbError> {
        self.check(scope, Action::UploadCreate)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Err(DbError::Internal);
        };
        if session.state != "created" {
            return Err(DbError::Internal);
        }
        let mut offset = 0i64;
        for (expected_ordinal, entry) in entries.iter().enumerate() {
            if entry.ordinal != i32::try_from(expected_ordinal).map_err(|_| DbError::Internal)?
                || entry.byte_offset != offset
                || !(1..=MAX_MANIFEST_CHUNK_BYTES).contains(&entry.size_bytes)
            {
                return Err(DbError::Internal);
            }
            offset = offset
                .checked_add(entry.size_bytes)
                .ok_or(DbError::Internal)?;
        }
        if offset != session.declared_size {
            return Err(DbError::Internal);
        }
        sqlx::query(
            "INSERT INTO upload_chunk_manifests (upload_id, file_id, created_at) VALUES ($1,$2,$3)",
        )
        .bind(id)
        .bind(session.file_id)
        .bind(now)
        .execute(&mut *self.tx)
        .await?;
        for entry in entries {
            sqlx::query(
                "INSERT INTO upload_chunk_manifest_entries (upload_id,ordinal,byte_offset,size_bytes,sha256,digest) \
                 VALUES ($1,$2,$3,$4,$5,$6)",
            )
            .bind(id)
            .bind(entry.ordinal)
            .bind(entry.byte_offset)
            .bind(entry.size_bytes)
            .bind(entry.sha256.as_slice())
            .bind(entry.digest.as_slice())
            .execute(&mut *self.tx)
            .await?;
        }
        Ok(())
    }

    /// Returns a same-file chunk candidate and locks it against GC/archive
    /// while the caller verifies its local bytes.
    pub async fn upload_manifest_candidate(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        ordinal: i32,
        digests: &[[u8; 32]],
        now: OffsetDateTime,
        action: Action,
    ) -> Result<Option<ManifestChunkCandidate>, DbError> {
        self.check(scope, action)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Ok(None);
        };
        let manifest_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM upload_chunk_manifest_entries WHERE upload_id=$1 AND ordinal=$2)",
        )
        .bind(id)
        .bind(ordinal)
        .fetch_one(&mut *self.tx)
        .await?;
        if !manifest_exists {
            return Ok(None);
        }
        for digest in digests {
            let candidate = sqlx::query_as::<_, ManifestChunkCandidate>(
                "SELECT id, digest, sha256, size_bytes, object_key, state, storage_tier, stored_bytes, encoding FROM chunks \
                 WHERE file_id=$1 AND digest=$2 AND state <> 'deleting' FOR UPDATE",
            )
            .bind(session.file_id)
            .bind(digest.as_slice())
            .fetch_optional(&mut *self.tx)
            .await?;
            if candidate.is_some() {
                return Ok(candidate);
            }
        }
        Ok(None)
    }

    /// Selects the matching key-version digest for this temporary ordinal.
    pub async fn upload_manifest_set_digest(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        ordinal: i32,
        digest: &[u8; 32],
    ) -> Result<bool, DbError> {
        self.check(scope, Action::UploadCreate)?;
        let updated = sqlx::query(
            "UPDATE upload_chunk_manifest_entries SET digest=$3 WHERE upload_id=$1 AND ordinal=$2",
        )
        .bind(id)
        .bind(ordinal)
        .bind(digest.as_slice())
        .execute(&mut *self.tx)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Makes a verified same-file chunk reachable from this session. The
    /// existing mapping is replaced atomically when a repaired object has a
    /// different chunk row.
    pub async fn upload_manifest_attach(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        ordinal: i32,
        chunk_id: Uuid,
        now: OffsetDateTime,
        action: Action,
    ) -> Result<bool, DbError> {
        self.check(scope, action)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Ok(false);
        };
        if !matches!(session.state.as_str(), "created" | "receiving") {
            return Ok(false);
        }
        let Some(offset) = sqlx::query_scalar::<_, i64>(
            "SELECT byte_offset FROM upload_chunk_manifest_entries WHERE upload_id=$1 AND ordinal=$2",
        )
        .bind(id)
        .bind(ordinal)
        .fetch_optional(&mut *self.tx)
        .await?
        else {
            return Ok(false);
        };
        let old: Option<Uuid> = sqlx::query_scalar(
            "SELECT chunk_id FROM upload_chunks WHERE upload_id=$1 AND ordinal=$2 FOR UPDATE",
        )
        .bind(id)
        .bind(ordinal)
        .fetch_optional(&mut *self.tx)
        .await?;
        if old == Some(chunk_id) {
            return Ok(true);
        }
        if old.is_some() {
            sqlx::query("DELETE FROM upload_chunks WHERE upload_id=$1 AND ordinal=$2")
                .bind(id)
                .bind(ordinal)
                .execute(&mut *self.tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO upload_chunks (upload_id,file_id,ordinal,chunk_id,byte_offset) \
             SELECT $1,$2,$3,$4,$5 WHERE EXISTS (SELECT 1 FROM chunks WHERE id=$4 AND file_id=$2 AND state <> 'deleting')",
        )
        .bind(id)
        .bind(session.file_id)
        .bind(ordinal)
        .bind(chunk_id)
        .bind(offset)
        .execute(&mut *self.tx)
        .await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM upload_chunks WHERE upload_id=$1 AND ordinal=$2 AND chunk_id=$3)",
        )
        .bind(id)
        .bind(ordinal)
        .bind(chunk_id)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(exists)
    }

    /// Returns ordered manifest entries and the chunks pinned to each ordinal.
    pub async fn upload_manifest_rows(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        action: Action,
    ) -> Result<Option<Vec<ChunkManifestRow>>, DbError> {
        self.check(scope, action)?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM upload_chunk_manifests WHERE upload_id=$1 AND file_id=(SELECT file_id FROM upload_sessions WHERE id=$1 AND org_id=$2))",
        )
        .bind(id)
        .bind(self.org_id)
        .fetch_one(&mut *self.tx)
        .await?;
        if !exists {
            return Ok(None);
        }
        Ok(Some(sqlx::query_as::<_, ChunkManifestRow>(
            "SELECT m.ordinal,m.byte_offset,m.size_bytes,m.sha256,m.digest,u.chunk_id,c.object_key,c.storage_tier,c.stored_bytes,c.encoding \
             FROM upload_chunk_manifest_entries m LEFT JOIN upload_chunks u ON u.upload_id=m.upload_id AND u.ordinal=m.ordinal \
             LEFT JOIN chunks c ON c.id=u.chunk_id AND c.file_id=(SELECT file_id FROM upload_chunk_manifests WHERE upload_id=m.upload_id) \
             WHERE m.upload_id=$1 ORDER BY m.ordinal",
        )
        .bind(id)
        .fetch_all(&mut *self.tx)
        .await?))
    }

    /// Replaces or inserts one verified chunk object, then pins it to the
    /// session ordinal. Returns the prior local key for post-commit deletion.
    // Keep the verified identity, byte length and candidate key together at this write boundary.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_manifest_store_chunk(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        ordinal: i32,
        digest: &[u8; 32],
        sha256: &[u8; 32],
        size_bytes: i64,
        stored_bytes: i64,
        encoding: &str,
        object_key: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Option<String>>, DbError> {
        self.check(scope, Action::UploadWrite)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Err(DbError::Internal);
        };
        if session.state != "created" {
            return Ok(None);
        }
        let expected: Option<(i64, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT size_bytes,sha256,digest FROM upload_chunk_manifest_entries WHERE upload_id=$1 AND ordinal=$2 FOR UPDATE",
        )
        .bind(id)
        .bind(ordinal)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((expected_size, expected_sha, expected_digest)) = expected else {
            return Ok(None);
        };
        if expected_size != size_bytes
            || expected_sha.as_slice() != sha256
            || expected_digest.as_slice() != digest
        {
            return Ok(None);
        }
        let existing: Option<(Uuid, String, String, Vec<u8>, i64, String)> = sqlx::query_as(
            "SELECT id,object_key,storage_tier,sha256,size_bytes,state FROM chunks WHERE file_id=$1 AND digest=$2 FOR UPDATE",
        )
        .bind(session.file_id)
        .bind(digest.as_slice())
        .fetch_optional(&mut *self.tx)
        .await?;
        let (chunk_id, prior_key) = if let Some((
            chunk_id,
            old_key,
            tier,
            old_sha,
            old_size,
            state,
        )) = existing
        {
            if state == "deleting" {
                return Ok(None);
            }
            if old_sha.as_slice() != sha256 || old_size != size_bytes {
                return Err(DbError::Internal);
            }
            sqlx::query(
            "UPDATE chunks SET object_key=$3,stored_bytes=$4,encoding=$5,state=CASE WHEN refcount>0 THEN 'committed' ELSE 'staged' END,storage_tier='local',archive_object_key=NULL,archive_wrapped_key=NULL,archive_verified_at=NULL, \
                 archive_claim_token=NULL,archive_claimed_at=NULL,archive_next_attempt_at=NULL, \
                 archive_error_code=NULL,archive_local_delete_pending=false \
                 WHERE id=$1 AND file_id=$2",
            )
            .bind(chunk_id)
            .bind(session.file_id)
            .bind(object_key)
            .bind(stored_bytes)
            .bind(encoding)
            .execute(&mut *self.tx)
            .await?;
            (
                chunk_id,
                (tier == "local" && old_key != object_key).then_some(old_key),
            )
        } else {
            let chunk_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO chunks (id,file_id,org_id,workspace_id,digest,sha256,object_key,size_bytes,stored_bytes,encoding,state,created_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'staged',$11)",
            )
            .bind(chunk_id)
            .bind(session.file_id)
            .bind(self.org_id)
            .bind(session.workspace_id)
            .bind(digest.as_slice())
            .bind(sha256.as_slice())
            .bind(object_key)
            .bind(size_bytes)
            .bind(stored_bytes)
            .bind(encoding)
            .bind(now)
            .execute(&mut *self.tx)
            .await?;
            (chunk_id, None)
        };
        let attached = self
            .upload_manifest_attach(scope, id, ordinal, chunk_id, now, Action::UploadWrite)
            .await?;
        if !attached {
            return Ok(None);
        }
        Ok(Some(prior_key))
    }

    /// Where a file lives and whether it or its project is in the trash:
    /// `(workspace_id, project_id, file_deleted, project_deleted)`.
    pub(crate) async fn file_state(
        &mut self,
        file_id: Uuid,
    ) -> Result<Option<(Uuid, Uuid, bool, bool)>, DbError> {
        Ok(sqlx::query_as(
            "SELECT f.workspace_id, f.project_id, f.deleted_at IS NOT NULL, \
                    p.deleted_at IS NOT NULL \
             FROM files f JOIN projects p ON p.id = f.project_id \
             WHERE f.id = $1 AND f.org_id = $2",
        )
        .bind(file_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    /// Gives reserved bytes back. A counter that is smaller than `bytes`
    /// (drift, for example from a database written before migration 0014 or by
    /// a bug) is clamped at zero instead of violating the non-negative CHECK,
    /// which would fail the whole request; the drift is logged and the usage
    /// reconciliation of the maintenance job repairs the counters from the rows.
    pub(crate) async fn release(&mut self, workspace_id: Uuid, bytes: i64) -> Result<(), DbError> {
        let ws: Option<i64> = sqlx::query_scalar(
            "SELECT reserved_bytes FROM workspaces WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let org: i64 = sqlx::query_scalar("SELECT reserved_bytes FROM organizations WHERE id = $1")
            .bind(self.org_id)
            .fetch_one(&mut *self.tx)
            .await?;
        if ws.is_none_or(|current| current < bytes) || org < bytes {
            tracing::warn!(
                target: "craftcloud::uploads",
                org = %self.org_id,
                workspace = %workspace_id,
                bytes,
                "a reserved-bytes counter was smaller than the release; clamped at zero"
            );
        }
        sqlx::query(
            "UPDATE workspaces SET reserved_bytes = GREATEST(reserved_bytes - $2, 0) \
             WHERE id = $1 AND org_id = $3",
        )
        .bind(workspace_id)
        .bind(bytes)
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?;
        sqlx::query(
            "UPDATE organizations SET reserved_bytes = GREATEST(reserved_bytes - $2, 0) WHERE id = $1",
        )
        .bind(self.org_id)
        .bind(bytes)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Adds bytes to the committed counters of a workspace and its organization.
    pub(crate) async fn add_committed(
        &mut self,
        quota_group_id: Uuid,
        workspace_id: Uuid,
        bytes: i64,
    ) -> Result<(), DbError> {
        let committed_copies: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM blobs WHERE org_id = $1 AND quota_group_id = $2 \
             AND state IN ('committed', 'corrupt')",
        )
        .bind(self.org_id)
        .bind(quota_group_id)
        .fetch_one(&mut *self.tx)
        .await?;
        if committed_copies != 1 {
            return Ok(());
        }
        sqlx::query(
            "UPDATE workspaces SET committed_bytes = committed_bytes + $2              WHERE id = $1 AND org_id = $3",
        )
        .bind(workspace_id)
        .bind(bytes)
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?;
        sqlx::query(
            "UPDATE organizations SET committed_bytes = committed_bytes + $2 WHERE id = $1",
        )
        .bind(self.org_id)
        .bind(bytes)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Closes a session that holds a reservation: new state, staged blob
    /// orphaned. The reservation is released at once unless a published object
    /// exists: the bytes of an `orphaned` blob stay reserved until the cleanup
    /// job removes it (see the module docs).
    pub(crate) async fn close_session(
        &mut self,
        session: &UploadSession,
        state: &str,
        now: OffsetDateTime,
    ) -> Result<UploadSession, DbError> {
        let updated = sqlx::query_as::<_, UploadSession>(&format!(
            "UPDATE upload_sessions SET state = $3, updated_at = $4, version = version + 1 \
             WHERE id = $1 AND org_id = $2 RETURNING {SESSION_COLUMNS}"
        ))
        .bind(session.id)
        .bind(self.org_id)
        .bind(state)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await?;
        let mut kept = false;
        if let Some(blob_id) = session.blob_id {
            kept = sqlx::query(
                "UPDATE blobs SET state = 'orphaned' WHERE id = $1 AND org_id = $2 AND state = 'staged'",
            )
            .bind(blob_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?
            .rows_affected()
                == 1;
        }
        if !kept {
            self.release(session.workspace_id, session.declared_size)
                .await?;
        }
        Ok(updated)
    }

    async fn session_row(&mut self, id: Uuid) -> Result<Option<UploadSession>, DbError> {
        Ok(sqlx::query_as::<_, UploadSession>(&format!(
            "SELECT {SESSION_COLUMNS} FROM upload_sessions WHERE id = $1 AND org_id = $2 FOR UPDATE"
        ))
        .bind(id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    /// Locks a session and expires it if its time is up (lazy expiry).
    pub(crate) async fn session_locked(
        &mut self,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<Option<UploadSession>, DbError> {
        let Some(session) = self.session_row(id).await? else {
            return Ok(None);
        };
        let holding = matches!(session.state.as_str(), "created" | "receiving" | "stored");
        if holding && session.expires_at <= now {
            return Ok(Some(self.close_session(&session, "expired", now).await?));
        }
        Ok(Some(session))
    }

    /// Expires every due session of this organization: releases reservations
    /// and orphans staged blobs. Returns how many sessions expired.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn expire_due(&mut self, now: OffsetDateTime) -> Result<u64, DbError> {
        let due = sqlx::query_as::<_, UploadSession>(&format!(
            "SELECT {SESSION_COLUMNS} FROM upload_sessions \
             WHERE org_id = $1 AND state IN {HOLDING} AND expires_at <= $2 \
             ORDER BY expires_at LIMIT 500 FOR UPDATE"
        ))
        .bind(self.org_id)
        .bind(now)
        .fetch_all(&mut *self.tx)
        .await?;
        let count = due.len() as u64;
        for session in due {
            self.close_session(&session, "expired", now).await?;
        }
        Ok(count)
    }

    /// Creates an upload session and reserves its size.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn upload_create(
        &mut self,
        scope: &OrgScope,
        new: &NewUpload,
    ) -> Result<UploadCreateOutcome, DbError> {
        self.check(scope, Action::UploadCreate)?;
        self.expire_due(new.now).await?;
        let Some((workspace_id, project_id, file_deleted, project_deleted)) =
            self.file_state(new.file_id).await?
        else {
            return Ok(UploadCreateOutcome::FileGone);
        };
        if project_deleted {
            return Ok(UploadCreateOutcome::ProjectDeleted);
        }
        if file_deleted {
            return Ok(UploadCreateOutcome::FileDeleted);
        }
        let print = fingerprint(new.file_id, new.size, &new.sha256);
        if let Some(key) = &new.idempotency_key {
            let existing = sqlx::query_as::<_, UploadSession>(&format!(
                "SELECT {SESSION_COLUMNS} FROM upload_sessions \
                 WHERE created_by_user_id = $1 AND idempotency_key = $2"
            ))
            .bind(new.user_id)
            .bind(key)
            .fetch_optional(&mut *self.tx)
            .await?;
            if let Some(existing) = existing {
                return Ok(if existing.request_fingerprint == print {
                    UploadCreateOutcome::Replayed(Box::new(existing))
                } else {
                    UploadCreateOutcome::KeyReused
                });
            }
        }
        // The per-user cap spans organizations: serialize the user's creations
        // (organization row first, see the module docs). `FOR NO KEY UPDATE`
        // serializes the cap without blocking inserts that only reference the
        // user (sessions, invitations).
        sqlx::query("SELECT id FROM users WHERE id = $1 FOR NO KEY UPDATE")
            .bind(new.user_id)
            .fetch_optional(&mut *self.tx)
            .await?;
        let by_user: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM upload_sessions \
             WHERE created_by_user_id = $1 AND state IN ('created', 'receiving')",
        )
        .bind(new.user_id)
        .fetch_one(&mut *self.tx)
        .await?;
        if by_user >= new.max_per_user {
            return Ok(UploadCreateOutcome::UserLimit);
        }
        let by_org: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM upload_sessions \
             WHERE org_id = $1 AND state IN ('created', 'receiving')",
        )
        .bind(self.org_id)
        .fetch_one(&mut *self.tx)
        .await?;
        if by_org >= new.max_per_org {
            return Ok(UploadCreateOutcome::OrgLimit);
        }
        // The organization row is locked by `begin`; lock the workspace row too.
        let workspace: Usage = sqlx::query_as(
            "SELECT quota_bytes, reserved_bytes, committed_bytes FROM workspaces \
             WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(self.org_id)
        .fetch_one(&mut *self.tx)
        .await?;
        let org: Usage = sqlx::query_as(
            "SELECT quota_bytes, reserved_bytes, committed_bytes FROM organizations WHERE id = $1",
        )
        .bind(self.org_id)
        .fetch_one(&mut *self.tx)
        .await?;
        for usage in [workspace, org] {
            let Some(quota) = usage.quota_bytes else {
                continue;
            };
            let used = usage
                .reserved_bytes
                .checked_add(usage.committed_bytes)
                .and_then(|u| u.checked_add(new.size));
            if used.is_none_or(|total| total > quota) {
                return Ok(UploadCreateOutcome::QuotaExceeded);
            }
        }
        sqlx::query(
            "UPDATE workspaces SET reserved_bytes = reserved_bytes + $2 WHERE id = $1 AND org_id = $3",
        )
        .bind(workspace_id)
        .bind(new.size)
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?;
        sqlx::query("UPDATE organizations SET reserved_bytes = reserved_bytes + $2 WHERE id = $1")
            .bind(self.org_id)
            .bind(new.size)
            .execute(&mut *self.tx)
            .await?;
        let inserted = sqlx::query_as::<_, UploadSession>(&format!(
            "INSERT INTO upload_sessions (id, org_id, workspace_id, project_id, file_id, \
                 created_by_user_id, declared_size, declared_sha256, idempotency_key, \
                 request_fingerprint, expires_at, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12) \
             RETURNING {SESSION_COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(self.org_id)
        .bind(workspace_id)
        .bind(project_id)
        .bind(new.file_id)
        .bind(new.user_id)
        .bind(new.size)
        .bind(new.sha256.as_slice())
        .bind(&new.idempotency_key)
        .bind(&print)
        .bind(new.expires_at)
        .bind(new.now)
        .fetch_one(&mut *self.tx)
        .await;
        match inserted {
            Ok(created) => Ok(UploadCreateOutcome::Created(Box::new(created))),
            // The same key used concurrently in another organization.
            Err(error) => match DbError::from(error) {
                e if e.constraint() == Some("upload_sessions_idempotency_idx") => {
                    Ok(UploadCreateOutcome::KeyReused)
                }
                e => Err(e),
            },
        }
    }

    /// Moves a `created` session to `receiving` (one request at a time).
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn upload_begin_receiving(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<BeginOutcome, DbError> {
        self.check(scope, Action::UploadWrite)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Ok(BeginOutcome::Gone);
        };
        match session.state.as_str() {
            "created" => {}
            "receiving" => return Ok(BeginOutcome::InProgress),
            "expired" => return Ok(BeginOutcome::Expired),
            other => return Ok(BeginOutcome::Closed(other.to_owned())),
        }
        match self.file_state(session.file_id).await? {
            Some((_, _, _, true)) => return Ok(BeginOutcome::ProjectDeleted),
            Some((_, _, true, _)) => return Ok(BeginOutcome::FileDeleted),
            _ => {}
        }
        let updated = sqlx::query_as::<_, UploadSession>(&format!(
            "UPDATE upload_sessions SET state = 'receiving', updated_at = $3, version = version + 1 \
             WHERE id = $1 AND org_id = $2 RETURNING {SESSION_COLUMNS}"
        ))
        .bind(id)
        .bind(self.org_id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(BeginOutcome::Started(Box::new(updated)))
    }

    /// Whether the login session is still valid for a write that completes
    /// now: the account is enabled, and the session is neither revoked nor past
    /// its absolute expiry. The rows are locked (`FOR SHARE`, users row first
    /// like `accounts::set_disabled`), so a concurrent account disable or
    /// session revoke either finished before this read or waits until the
    /// transaction ends. Idleness is not checked: the upload itself is activity.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn session_valid(
        &mut self,
        session_id: Uuid,
        user_id: Uuid,
        now: OffsetDateTime,
        device: bool,
    ) -> Result<bool, DbError> {
        let enabled: Option<bool> =
            sqlx::query_scalar("SELECT disabled_at IS NULL FROM users WHERE id = $1 FOR SHARE")
                .bind(user_id)
                .fetch_optional(&mut *self.tx)
                .await?;
        if enabled != Some(true) {
            return Ok(false);
        }
        let created: Option<OffsetDateTime> = if device {
            sqlx::query_scalar("SELECT created_at FROM device_sessions WHERE id=$1 AND user_id=$2")
                .bind(session_id)
                .bind(user_id)
                .fetch_optional(&mut *self.tx)
                .await?
        } else {
            sqlx::query_scalar("SELECT created_at FROM sessions WHERE id=$1 AND user_id=$2")
                .bind(session_id)
                .bind(user_id)
                .fetch_optional(&mut *self.tx)
                .await?
        };
        let Some(created) = created else {
            return Ok(false);
        };
        let (mfa, sso) = match crate::sso::session_policy_in(
            &mut self.tx,
            self.org_id,
            user_id,
            if device { "device" } else { "browser" },
            session_id,
            created,
        )
        .await?
        {
            crate::sso::SessionPolicy::Allowed { mfa, sso } => (mfa, sso),
            crate::sso::SessionPolicy::SsoRequired | crate::sso::SessionPolicy::Revoked => {
                return Ok(false);
            }
        };
        let require_two_factor: bool =
            sqlx::query_scalar("SELECT require_two_factor FROM organizations WHERE id=$1")
                .bind(self.org_id)
                .fetch_one(&mut *self.tx)
                .await?;
        if require_two_factor && sso && !mfa {
            return Ok(false);
        }
        if require_two_factor && !mfa {
            let factor_enabled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)")
                .bind(user_id).fetch_one(&mut *self.tx).await?;
            if !factor_enabled {
                return Ok(false);
            }
        }
        let live: Option<Uuid> = if device {
            sqlx::query_scalar("SELECT id FROM device_sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE")
                .bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *self.tx).await?
        } else {
            sqlx::query_scalar("SELECT id FROM sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE")
                .bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *self.tx).await?
        };
        Ok(live.is_some())
    }

    /// Classifies an organization denial without treating a still-valid global
    /// credential as logged out of every tenant.
    pub async fn session_organization_policy(
        &mut self,
        session: Uuid,
        user: Uuid,
        device: bool,
        now: OffsetDateTime,
    ) -> Result<Option<(crate::sso::SessionPolicy, bool)>, DbError> {
        let query = if device {
            "SELECT d.created_at FROM device_sessions d JOIN users u ON u.id=d.user_id WHERE d.id=$1 AND d.user_id=$2 AND d.revoked_at IS NULL AND d.absolute_expires_at>$3 AND u.disabled_at IS NULL FOR SHARE OF d"
        } else {
            "SELECT d.created_at FROM sessions d JOIN users u ON u.id=d.user_id WHERE d.id=$1 AND d.user_id=$2 AND d.revoked_at IS NULL AND d.absolute_expires_at>$3 AND u.disabled_at IS NULL FOR SHARE OF d"
        };
        let created: Option<OffsetDateTime> = sqlx::query_scalar(query)
            .bind(session)
            .bind(user)
            .bind(now)
            .fetch_optional(&mut *self.tx)
            .await?;
        let Some(created) = created else {
            return Ok(None);
        };
        let policy = crate::sso::session_policy_in(
            &mut self.tx,
            self.org_id,
            user,
            if device { "device" } else { "browser" },
            session,
            created,
        )
        .await?;
        let required = self.require_two_factor().await?;
        Ok(Some((policy, required)))
    }

    pub async fn two_factor_setup_required(&mut self, user_id: Uuid) -> Result<bool, DbError> {
        let required: bool =
            sqlx::query_scalar("SELECT require_two_factor FROM organizations WHERE id=$1")
                .bind(self.org_id)
                .fetch_one(&mut *self.tx)
                .await?;
        if !required {
            return Ok(false);
        }
        let enabled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)")
            .bind(user_id).fetch_one(&mut *self.tx).await?;
        Ok(!enabled)
    }

    /// Records the published object as a staged blob and the session as
    /// `stored`. The caller has already decided with fresh roles that the
    /// actor may still write.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn upload_store(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        object_key: &str,
        now: OffsetDateTime,
    ) -> Result<StoreOutcome, DbError> {
        self.upload_store_inner(scope, id, object_key, now, None)
            .await
            .map(|(outcome, _, _)| outcome)
    }

    /// Stores a logical blob and its file-scoped chunk manifest.
    /// Returns candidate object keys that lost a same-file deduplication race.
    pub async fn upload_store_chunked(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        object_key: &str,
        now: OffsetDateTime,
        chunks: &[NewChunk],
    ) -> Result<(StoreOutcome, Vec<String>, Vec<ChunkReuseCandidate>), DbError> {
        self.upload_store_inner(scope, id, object_key, now, Some(chunks))
            .await
    }

    /// Publishes the logical blob for a complete Phase-B manifest. The caller
    /// has already verified the assembled whole-file SHA-256 outside this
    /// transaction; every ordinal remains pinned by `upload_chunks` here.
    pub async fn upload_store_chunk_manifest(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        object_key: &str,
        now: OffsetDateTime,
    ) -> Result<StoreOutcome, DbError> {
        self.check(scope, Action::VersionCommit)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Ok(StoreOutcome::Gone);
        };
        if session.state != "created" {
            return Ok(StoreOutcome::NotReceiving(session.state));
        }
        match self.file_state(session.file_id).await? {
            Some((_, _, _, true)) => return Ok(StoreOutcome::ProjectDeleted),
            Some((_, _, true, _)) => return Ok(StoreOutcome::FileDeleted),
            _ => {}
        }
        let ready: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM upload_chunk_manifests h WHERE h.upload_id=$1 AND h.file_id=$2) \
             AND (SELECT count(*) FROM upload_chunk_manifest_entries WHERE upload_id=$1) = \
                 (SELECT count(*) FROM upload_chunks WHERE upload_id=$1) \
             AND NOT EXISTS (SELECT 1 FROM upload_chunk_manifest_entries m \
                 LEFT JOIN upload_chunks u ON u.upload_id=m.upload_id AND u.ordinal=m.ordinal \
                 LEFT JOIN chunks c ON c.id=u.chunk_id AND c.file_id=$2 \
                 WHERE m.upload_id=$1 AND (c.id IS NULL OR c.size_bytes <> m.size_bytes \
                   OR c.sha256 <> m.sha256 OR c.state='deleting' OR c.storage_tier <> 'local')) \
             AND COALESCE((SELECT sum(size_bytes) FROM upload_chunk_manifest_entries WHERE upload_id=$1),0)=$3",
        )
        .bind(id)
        .bind(session.file_id)
        .bind(session.declared_size)
        .fetch_one(&mut *self.tx)
        .await?;
        if !ready {
            return Ok(StoreOutcome::NotReceiving("created".to_owned()));
        }
        let blob_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO blob_quota_groups (id, org_id, workspace_id, size_bytes) VALUES ($1,$2,$3,$4)",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(session.workspace_id)
        .bind(session.declared_size)
        .execute(&mut *self.tx)
        .await?;
        let blob = sqlx::query_as::<_, Blob>(
            "INSERT INTO blobs (id,org_id,workspace_id,quota_group_id,object_key,size_bytes,sha256,layout,created_at) \
             VALUES ($1,$2,$3,$1,$4,$5,$6,'chunked',$7) \
             RETURNING id,org_id,workspace_id,quota_group_id,object_key,size_bytes,sha256,state,created_at",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(session.workspace_id)
        .bind(object_key)
        .bind(session.declared_size)
        .bind(&session.declared_sha256)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await?;
        let updated = sqlx::query_as::<_, UploadSession>(&format!(
            "UPDATE upload_sessions SET state='stored',blob_id=$3,updated_at=$4,version=version+1 \
             WHERE id=$1 AND org_id=$2 RETURNING {SESSION_COLUMNS}"
        ))
        .bind(id)
        .bind(self.org_id)
        .bind(blob.id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(StoreOutcome::Stored(Box::new(updated), Box::new(blob)))
    }

    async fn upload_store_inner(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        object_key: &str,
        now: OffsetDateTime,
        chunks: Option<&[NewChunk]>,
    ) -> Result<(StoreOutcome, Vec<String>, Vec<ChunkReuseCandidate>), DbError> {
        self.check(scope, Action::UploadWrite)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Ok((StoreOutcome::Gone, Vec::new(), Vec::new()));
        };
        if session.state != "receiving" {
            return Ok((
                StoreOutcome::NotReceiving(session.state),
                Vec::new(),
                Vec::new(),
            ));
        }
        match self.file_state(session.file_id).await? {
            Some((_, _, _, true)) => {
                return Ok((StoreOutcome::ProjectDeleted, Vec::new(), Vec::new()));
            }
            Some((_, _, true, _)) => {
                return Ok((StoreOutcome::FileDeleted, Vec::new(), Vec::new()));
            }
            _ => {}
        }
        let blob_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO blob_quota_groups (id, org_id, workspace_id, size_bytes) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(session.workspace_id)
        .bind(session.declared_size)
        .execute(&mut *self.tx)
        .await?;
        let blob = sqlx::query_as::<_, Blob>(
            "INSERT INTO blobs (id, org_id, workspace_id, quota_group_id, object_key, size_bytes, sha256, layout, created_at) \
             VALUES ($1, $2, $3, $1, $4, $5, $6, $8, $7) \
             RETURNING id, org_id, workspace_id, quota_group_id, object_key, size_bytes, sha256, state, created_at",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(session.workspace_id)
        .bind(object_key)
        .bind(session.declared_size)
        .bind(&session.declared_sha256)
        .bind(now)
        .bind(if chunks.is_some() { "chunked" } else { "whole" })
        .fetch_one(&mut *self.tx)
        .await?;
        let mut unused_keys = Vec::new();
        let mut local_reuse_candidates = Vec::new();
        if let Some(chunks) = chunks {
            let mut previous_offset = 0i64;
            for (ordinal, chunk) in chunks.iter().enumerate() {
                if chunk.byte_offset != previous_offset || chunk.size_bytes <= 0 {
                    return Err(DbError::Internal);
                }
                previous_offset = previous_offset
                    .checked_add(chunk.size_bytes)
                    .ok_or(DbError::Internal)?;
                let mut existing_chunk: Option<ExistingChunkRow> = None;
                for digest in &chunk.digest_candidates {
                    existing_chunk = sqlx::query_as(
                        "SELECT id, size_bytes, sha256, state, storage_tier, object_key, encoding, stored_bytes FROM chunks WHERE file_id = $1 AND digest = $2 FOR UPDATE",
                    )
                    .bind(session.file_id)
                    .bind(digest.as_slice())
                    .fetch_optional(&mut *self.tx)
                    .await?;
                    if existing_chunk.is_some() {
                        break;
                    }
                }
                let chunk_id = if let Some((
                    existing_id,
                    existing_size,
                    existing_sha,
                    existing_state,
                    existing_tier,
                    existing_key,
                    existing_encoding,
                    existing_stored_bytes,
                )) = existing_chunk
                {
                    if existing_size != chunk.size_bytes
                        || existing_sha.as_slice() != chunk.sha256
                        || existing_state == "deleting"
                        || !matches!(existing_tier.as_str(), "local" | "archiving" | "archive")
                    {
                        return Err(DbError::Internal);
                    }
                    if existing_tier == "local" {
                        unused_keys.push(chunk.object_key.clone());
                        if !local_reuse_candidates
                            .iter()
                            .any(|reuse: &ChunkReuseCandidate| reuse.chunk_id == existing_id)
                        {
                            local_reuse_candidates.push(ChunkReuseCandidate {
                                chunk_id: existing_id,
                                existing_object_key: existing_key,
                                candidate_object_key: chunk.object_key.clone(),
                                sha256: chunk.sha256,
                                size_bytes: chunk.size_bytes,
                                stored_bytes: existing_stored_bytes,
                                encoding: existing_encoding,
                                candidate_encoding: chunk.encoding.clone(),
                                candidate_stored_bytes: chunk.stored_bytes,
                            });
                        }
                    } else {
                        // The candidate bytes are a verified local replacement. Fencing the
                        // archive worker before commit prevents a later delete from removing
                        // a chunk used by the new head.
                        sqlx::query(
                            "UPDATE chunks SET object_key=$2, encoding=$4, stored_bytes=$5, storage_tier='local', archive_object_key=NULL,archive_wrapped_key=NULL,archive_verified_at=NULL,archive_error_code=NULL, \
                                 archive_local_delete_pending=false, archive_claim_token=NULL, \
                                 archive_claimed_at=NULL, archive_next_attempt_at=NULL \
                             WHERE id=$1 AND file_id=$3",
                        )
                        .bind(existing_id)
                        .bind(&chunk.object_key)
                        .bind(session.file_id)
                        .bind(&chunk.encoding)
                        .bind(chunk.stored_bytes)
                        .execute(&mut *self.tx)
                        .await?;
                        unused_keys.push(existing_key);
                    }
                    existing_id
                } else {
                    let candidate_id = Uuid::new_v4();
                    let inserted: Option<Uuid> = sqlx::query_scalar(
                        "INSERT INTO chunks (id, file_id, org_id, workspace_id, digest, sha256, object_key, size_bytes, encoding, stored_bytes, state, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'staged', $11) \
                     ON CONFLICT (file_id, digest) DO NOTHING RETURNING id",
                    )
                    .bind(candidate_id)
                    .bind(session.file_id)
                    .bind(self.org_id)
                    .bind(session.workspace_id)
                    .bind(chunk.digest.as_slice())
                    .bind(chunk.sha256.as_slice())
                    .bind(&chunk.object_key)
                    .bind(chunk.size_bytes)
                    .bind(&chunk.encoding)
                    .bind(chunk.stored_bytes)
                    .bind(now)
                    .fetch_optional(&mut *self.tx)
                    .await?;
                    if let Some(inserted) = inserted {
                        inserted
                    } else {
                        let existing: Option<ExistingChunkRow> = sqlx::query_as(
                            "SELECT id, size_bytes, sha256, state, storage_tier, object_key, encoding, stored_bytes FROM chunks WHERE file_id = $1 AND digest = $2 FOR UPDATE",
                        )
                        .bind(session.file_id)
                        .bind(chunk.digest.as_slice())
                        .fetch_optional(&mut *self.tx)
                        .await?;
                        let Some((
                            existing_id,
                            existing_size,
                            existing_sha,
                            existing_state,
                            existing_tier,
                            existing_key,
                            existing_encoding,
                            existing_stored_bytes,
                        )) = existing
                        else {
                            return Err(DbError::Internal);
                        };
                        if existing_size != chunk.size_bytes
                            || existing_sha.as_slice() != chunk.sha256
                            || existing_state == "deleting"
                            || !matches!(existing_tier.as_str(), "local" | "archiving" | "archive")
                        {
                            return Err(DbError::Internal);
                        }
                        if existing_tier == "local" {
                            unused_keys.push(chunk.object_key.clone());
                            if !local_reuse_candidates
                                .iter()
                                .any(|reuse: &ChunkReuseCandidate| reuse.chunk_id == existing_id)
                            {
                                local_reuse_candidates.push(ChunkReuseCandidate {
                                    chunk_id: existing_id,
                                    existing_object_key: existing_key,
                                    candidate_object_key: chunk.object_key.clone(),
                                    sha256: chunk.sha256,
                                    size_bytes: chunk.size_bytes,
                                    stored_bytes: existing_stored_bytes,
                                    encoding: existing_encoding,
                                    candidate_encoding: chunk.encoding.clone(),
                                    candidate_stored_bytes: chunk.stored_bytes,
                                });
                            }
                        } else {
                            sqlx::query(
                                "UPDATE chunks SET object_key=$2, encoding=$4, stored_bytes=$5, storage_tier='local', archive_object_key=NULL,archive_wrapped_key=NULL,archive_verified_at=NULL,archive_error_code=NULL, \
                                     archive_local_delete_pending=false, archive_claim_token=NULL, \
                                     archive_claimed_at=NULL, archive_next_attempt_at=NULL \
                                 WHERE id=$1 AND file_id=$3",
                            )
                            .bind(existing_id)
                            .bind(&chunk.object_key)
                            .bind(session.file_id)
                            .bind(&chunk.encoding)
                            .bind(chunk.stored_bytes)
                            .execute(&mut *self.tx)
                            .await?;
                            unused_keys.push(existing_key);
                        }
                        existing_id
                    }
                };
                sqlx::query(
                    "INSERT INTO upload_chunks (upload_id, file_id, ordinal, chunk_id, byte_offset) \
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(id)
                .bind(session.file_id)
                .bind(i32::try_from(ordinal).map_err(|_| DbError::Internal)?)
                .bind(chunk_id)
                .bind(chunk.byte_offset)
                .execute(&mut *self.tx)
                .await?;
            }
            if previous_offset != session.declared_size {
                return Err(DbError::Internal);
            }
        }
        let updated = sqlx::query_as::<_, UploadSession>(&format!(
            "UPDATE upload_sessions SET state = 'stored', blob_id = $3, updated_at = $4, \
                 version = version + 1 \
             WHERE id = $1 AND org_id = $2 RETURNING {SESSION_COLUMNS}"
        ))
        .bind(id)
        .bind(self.org_id)
        .bind(blob.id)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok((
            StoreOutcome::Stored(Box::new(updated), Box::new(blob)),
            unused_keys,
            local_reuse_candidates,
        ))
    }

    /// Replaces a selected local chunk object with the verified candidate
    /// uploaded by this session. The caller holds the same organization
    /// transaction that selected and locked the chunk row.
    pub async fn upload_replace_chunk_object(
        &mut self,
        scope: &OrgScope,
        upload_id: Uuid,
        reuse: &ChunkReuseCandidate,
    ) -> Result<bool, DbError> {
        self.check(scope, Action::UploadWrite)?;
        let done = sqlx::query(
            "UPDATE chunks SET object_key=$4, encoding=$8, stored_bytes=$9, archive_object_key=NULL, archive_wrapped_key=NULL, archive_verified_at=NULL, archive_claim_token=NULL, archive_claimed_at=NULL, archive_next_attempt_at=NULL, archive_error_code=NULL, archive_local_delete_pending=false WHERE id=$1 AND org_id=$2 \
             AND object_key=$3 AND storage_tier='local' AND state <> 'deleting' \
             AND size_bytes=$6 AND sha256=$7 \
             AND EXISTS (SELECT 1 FROM upload_chunks u WHERE u.upload_id=$5 AND u.chunk_id=chunks.id)",
        )
        .bind(reuse.chunk_id)
        .bind(self.org_id)
        .bind(&reuse.existing_object_key)
        .bind(&reuse.candidate_object_key)
        .bind(upload_id)
        .bind(reuse.size_bytes)
        .bind(reuse.sha256.as_slice())
        .bind(&reuse.candidate_encoding)
        .bind(reuse.candidate_stored_bytes)
        .execute(&mut *self.tx)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Marks a session `failed` and releases its reservation (lifecycle path:
    /// it must work even when the actor has lost their rights). Returns
    /// whether the session was still open.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn upload_fail(&mut self, id: Uuid, now: OffsetDateTime) -> Result<bool, DbError> {
        let Some(session) = self.session_locked(id, now).await? else {
            return Ok(false);
        };
        if !matches!(session.state.as_str(), "created" | "receiving") {
            return Ok(false);
        }
        self.close_session(&session, "failed", now).await?;
        Ok(true)
    }

    /// Aborts a session that is not receiving. A `created` session releases
    /// its reservation; a `stored` one keeps counting until its object is
    /// deleted (see the module docs).
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn upload_abort(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<AbortOutcome, DbError> {
        self.check(scope, Action::UploadAbort)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Ok(AbortOutcome::Gone);
        };
        match session.state.as_str() {
            "created" | "stored" => {
                let updated = self.close_session(&session, "aborted", now).await?;
                Ok(AbortOutcome::Aborted(Box::new(updated)))
            }
            "receiving" => Ok(AbortOutcome::InProgress),
            "committed" => Ok(AbortOutcome::Committed),
            _ => Ok(AbortOutcome::Unchanged(Box::new(session))),
        }
    }

    /// Sets the storage quota of the organization (operator action; the
    /// operator permit is checked by the caller). Existing data is never
    /// touched: a lower quota only blocks new reservations.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn org_quota_set(&mut self, quota: Option<i64>) -> Result<bool, DbError> {
        let changed =
            sqlx::query("UPDATE organizations SET quota_bytes = $2 WHERE id = $1 AND quota_bytes IS DISTINCT FROM $2")
                .bind(self.org_id)
                .bind(quota)
                .execute(&mut *self.tx)
                .await?
                .rows_affected();
        Ok(changed == 1)
    }

    /// Sets the storage quota of a workspace; a quota above the organization
    /// quota is refused.
    ///
    /// # Errors
    ///
    /// Database failures; scope mismatches are internal errors.
    pub async fn workspace_quota_set(
        &mut self,
        scope: &OrgScope,
        workspace_id: Uuid,
        quota: Option<i64>,
    ) -> Result<QuotaSetOutcome, DbError> {
        self.check(scope, Action::WorkspaceQuotaSet)?;
        if !self.workspace_exists(workspace_id).await? {
            return Ok(QuotaSetOutcome::Gone);
        }
        if let Some(requested) = quota {
            let org_quota: Option<i64> =
                sqlx::query_scalar("SELECT quota_bytes FROM organizations WHERE id = $1")
                    .bind(self.org_id)
                    .fetch_one(&mut *self.tx)
                    .await?;
            if org_quota.is_some_and(|limit| requested > limit) {
                return Ok(QuotaSetOutcome::ExceedsOrganization);
            }
        }
        let changed = sqlx::query(
            "UPDATE workspaces SET quota_bytes = $3              WHERE id = $1 AND org_id = $2 AND quota_bytes IS DISTINCT FROM $3",
        )
        .bind(workspace_id)
        .bind(self.org_id)
        .bind(quota)
        .execute(&mut *self.tx)
        .await?
        .rows_affected();
        Ok(if changed == 1 {
            QuotaSetOutcome::Set
        } else {
            QuotaSetOutcome::Unchanged
        })
    }

    /// Recomputes the usage counters of the organization and its workspaces
    /// from the sessions and blobs (reconciliation).
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn recompute_usage(&mut self) -> Result<(), DbError> {
        sqlx::query(&format!(
            "UPDATE workspaces w SET \
                reserved_bytes = COALESCE((SELECT sum(declared_size) FROM upload_sessions s \
                    WHERE s.workspace_id = w.id AND s.state IN {HOLDING}), 0) \
                    + COALESCE((SELECT sum(size_bytes) FROM blobs b \
                    WHERE b.workspace_id = w.id AND b.state = 'orphaned'), 0), \
                committed_bytes = COALESCE((SELECT sum(q.size_bytes) FROM blob_quota_groups q \
                    WHERE q.org_id = w.org_id AND q.workspace_id = w.id \
                      AND EXISTS (SELECT 1 FROM blobs b WHERE b.quota_group_id = q.id \
                        AND b.org_id = q.org_id AND b.workspace_id = q.workspace_id \
                        AND b.state IN ('committed', 'corrupt'))), 0) \
             WHERE w.org_id = $1"
        ))
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?;
        sqlx::query(
            "UPDATE organizations o SET \
                reserved_bytes = COALESCE((SELECT sum(reserved_bytes) FROM workspaces w WHERE w.org_id = o.id), 0), \
                committed_bytes = COALESCE((SELECT sum(committed_bytes) FROM workspaces w WHERE w.org_id = o.id), 0) \
             WHERE o.id = $1",
        )
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }
}

/// Expires due sessions of every organization, one organization at a time
/// (for the cleanup job); returns the number of expired sessions. At most
/// `max_orgs` organizations are processed per call.
///
/// # Errors
///
/// Database failures only.
pub async fn expire_due_all(
    pool: &PgPool,
    now: OffsetDateTime,
    max_orgs: i64,
) -> Result<u64, DbError> {
    let orgs: Vec<Uuid> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT org_id FROM upload_sessions \
         WHERE state IN {HOLDING} AND expires_at <= $1 LIMIT $2"
    ))
    .bind(now)
    .bind(max_orgs)
    .fetch_all(pool)
    .await?;
    let mut total = 0;
    for org_id in orgs {
        if let Some(mut tx) = OrgTx::begin(pool, org_id).await? {
            total += tx.expire_due(now).await?;
            tx.commit().await?;
        }
    }
    Ok(total)
}

/// Staged blobs whose session ended without a commit (and are orphaned) are
/// listed for the cleanup job: object keys, oldest first.
///
/// # Errors
///
/// Database failures only.
pub async fn orphaned_blobs(pool: &PgPool, limit: i64) -> Result<Vec<(Uuid, String)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id, object_key FROM blobs WHERE state = 'orphaned' ORDER BY created_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
