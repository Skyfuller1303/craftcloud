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

/// Where a session lives, resolved from its row.
#[derive(Debug, Clone, Copy)]
pub struct SessionLocation {
    /// Organization.
    pub org_id: Uuid,
    /// Workspace.
    pub workspace_id: Uuid,
    /// Who created it.
    pub creator: Uuid,
}

/// Resolves organization, workspace and creator of an upload id.
///
/// # Errors
///
/// Database failures only.
pub async fn locate(pool: &PgPool, id: Uuid) -> Result<Option<SessionLocation>, DbError> {
    let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT org_id, workspace_id, created_by_user_id FROM upload_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(org_id, workspace_id, creator)| SessionLocation {
        org_id,
        workspace_id,
        creator,
    }))
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
    /// Where a file lives and whether it or its project is in the trash:
    /// `(workspace_id, project_id, file_deleted, project_deleted)`.
    async fn file_state(
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
        workspace_id: Uuid,
        bytes: i64,
    ) -> Result<(), DbError> {
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
    ) -> Result<bool, DbError> {
        let enabled: Option<bool> =
            sqlx::query_scalar("SELECT disabled_at IS NULL FROM users WHERE id = $1 FOR SHARE")
                .bind(user_id)
                .fetch_optional(&mut *self.tx)
                .await?;
        if enabled != Some(true) {
            return Ok(false);
        }
        let live: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM sessions WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL \
             AND absolute_expires_at > $3 FOR SHARE",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(now)
        .fetch_optional(&mut *self.tx)
        .await?;
        Ok(live.is_some())
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
        self.check(scope, Action::UploadWrite)?;
        let Some(session) = self.session_locked(id, now).await? else {
            return Ok(StoreOutcome::Gone);
        };
        if session.state != "receiving" {
            return Ok(StoreOutcome::NotReceiving(session.state));
        }
        match self.file_state(session.file_id).await? {
            Some((_, _, _, true)) => return Ok(StoreOutcome::ProjectDeleted),
            Some((_, _, true, _)) => return Ok(StoreOutcome::FileDeleted),
            _ => {}
        }
        let blob = sqlx::query_as::<_, Blob>(
            "INSERT INTO blobs (id, org_id, workspace_id, object_key, size_bytes, sha256, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING id, org_id, workspace_id, object_key, size_bytes, sha256, state, created_at",
        )
        .bind(Uuid::new_v4())
        .bind(self.org_id)
        .bind(session.workspace_id)
        .bind(object_key)
        .bind(session.declared_size)
        .bind(&session.declared_sha256)
        .bind(now)
        .fetch_one(&mut *self.tx)
        .await?;
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
        Ok(StoreOutcome::Stored(Box::new(updated), Box::new(blob)))
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
                committed_bytes = COALESCE((SELECT sum(size_bytes) FROM blobs b \
                    WHERE b.workspace_id = w.id AND b.state IN ('committed', 'corrupt')), 0) \
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
