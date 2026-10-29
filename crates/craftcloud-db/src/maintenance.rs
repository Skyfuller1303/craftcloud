//! Database side of the maintenance job (CC-F10, CC-P02-06, CC-P02-07):
//! retention purge, version retention, blob garbage collection with a claim
//! phase, integrity bookkeeping, usage drift correction and the run log.
//!
//! These are lifecycle paths without an acting user: they take no
//! [`crate::OrgScope`]. Their safety comes from the rules below instead of from
//! a policy decision, and the server only calls them from the maintenance job
//! (and its tests).
//!
//! - Every change of tenant data happens in an [`OrgTx`]: organization row
//!   first, then the rows below it, with all state re-checked under the lock.
//!   The scans that pick candidates run without locks and are only hints.
//! - **Lock order** is the one of the rest of the crate (organization, then
//!   file or blob rows, then workspace/organization counters). The advisory run
//!   lock ([`try_lock`]) is taken before any of them and never while holding
//!   one.
//! - **Blob garbage collection** has two phases. The claim
//!   ([`OrgTx::maint_gc_claim`]) re-checks under the organization lock that no
//!   version and no open upload session references the blob, sets `deleting`,
//!   detaches closed sessions and, if the blob was counted, decrements
//!   `committed_bytes` exactly once (the counter never drops below zero). Only
//!   then the caller deletes the object and finally the row
//!   ([`gc_finish`]). A crash in between leaves a `deleting` row that the next
//!   run completes ([`gc_deleting`]); nothing may reference a `deleting` blob
//!   (commit and restore check this under the lock).
//! - Timestamps and cut-offs come from the caller (the injectable clock).

use std::collections::HashSet;

use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, Detail, NewEvent};
use crate::members::OrgTx;
use crate::uploads::{HOLDING, SESSION_COLUMNS, UploadSession};

/// Key of the advisory lock that admits one maintenance run at a time ("CCMAINT").
const RUN_LOCK_KEY: i64 = 0x4343_4D41_494E_5400;

/// The number of finished runs kept in `maintenance_runs`.
const KEEP_RUNS: i64 = 100;

// ---- run lock ---------------------------------------------------------------

/// Proof that this process is the only maintenance runner. The lock is a
/// session-level advisory lock on a dedicated pooled connection (no open
/// transaction). [`RunLock::release`] unlocks explicitly; dropping the value
/// (a panic, a cancelled task) closes the connection instead of returning it
/// to the pool, which frees the lock as well.
pub struct RunLock {
    conn: Option<PoolConnection<Postgres>>,
}

impl RunLock {
    /// Frees the lock now and returns the connection to the pool.
    ///
    /// # Errors
    ///
    /// Database failures only; the connection is then closed, which frees the
    /// lock anyway.
    pub async fn release(mut self) -> Result<(), DbError> {
        if let Some(mut conn) = self.conn.take() {
            let unlocked: Result<bool, sqlx::Error> =
                sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
                    .bind(RUN_LOCK_KEY)
                    .fetch_one(&mut *conn)
                    .await;
            match unlocked {
                Ok(true) => {}
                Ok(false) => {
                    conn.close_on_drop();
                }
                Err(error) => {
                    conn.close_on_drop();
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if let Some(mut conn) = self.conn.take() {
            // Not released explicitly: do not hand a locked session back to the pool.
            conn.close_on_drop();
        }
    }
}

/// Tries to become the maintenance runner (PostgreSQL advisory lock, shared by
/// all server processes and the CLI). `None` if another run holds it.
///
/// # Errors
///
/// Database failures only.
pub async fn try_lock(pool: &PgPool) -> Result<Option<RunLock>, DbError> {
    let mut conn = pool.acquire().await?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(RUN_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await?;
    if got {
        Ok(Some(RunLock { conn: Some(conn) }))
    } else {
        Ok(None)
    }
}

// ---- retention purge --------------------------------------------------------

/// What a purge removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PurgeCounts {
    /// File rows.
    pub files: u64,
    /// Version rows.
    pub versions: u64,
    /// Upload session rows.
    pub sessions: u64,
    /// Project rows.
    pub projects: u64,
}

impl PurgeCounts {
    /// Adds `other` to `self`.
    pub fn add(&mut self, other: Self) {
        self.files += other.files;
        self.versions += other.versions;
        self.sessions += other.sessions;
        self.projects += other.projects;
    }
}

/// Number of upload sessions that are open and past their time (dry run of the
/// expiry step).
///
/// # Errors
///
/// Database failures only.
pub async fn count_due_sessions(pool: &PgPool, now: OffsetDateTime) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT count(*) FROM upload_sessions WHERE state IN {HOLDING} AND expires_at <= $1"
    ))
    .bind(now)
    .fetch_one(pool)
    .await?)
}

/// Number of upload sessions that are open (created, receiving or stored but
/// not yet committed), over all organizations. An instance-wide aggregate for
/// the operator health view and the metrics; it carries no tenant data.
///
/// # Errors
///
/// Database failures only.
pub async fn count_open_sessions(pool: &PgPool) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT count(*) FROM upload_sessions WHERE state IN {HOLDING}"
    ))
    .fetch_one(pool)
    .await?)
}

/// Files in the trash since before `cutoff`: `(org_id, file_id)`, oldest first.
///
/// # Errors
///
/// Database failures only.
pub async fn trashed_files_due(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT org_id, id FROM files WHERE deleted_at IS NOT NULL AND deleted_at < $1 \
           AND id <> ALL($3) \
         ORDER BY deleted_at, id LIMIT $2",
    )
    .bind(cutoff)
    .bind(limit)
    .bind(exclude)
    .fetch_all(pool)
    .await?)
}

/// Projects in the trash since before `cutoff`: `(org_id, project_id)`, oldest first.
///
/// # Errors
///
/// Database failures only.
pub async fn trashed_projects_due(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT org_id, id FROM projects WHERE deleted_at IS NOT NULL AND deleted_at < $1 \
           AND id <> ALL($3) \
         ORDER BY deleted_at, id LIMIT $2",
    )
    .bind(cutoff)
    .bind(limit)
    .bind(exclude)
    .fetch_all(pool)
    .await?)
}

/// Files that have versions older than the retention: `(org_id, file_id)`.
/// A version qualifies when it is not the head and the version that followed
/// it was created before `cutoff`. Files and projects in the trash are left
/// alone (the trash purge removes them whole). Only organizations in `orgs`
/// are looked at (the caller walks the organizations in windows).
///
/// # Errors
///
/// Database failures only.
pub async fn files_with_old_versions(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    orgs: &[Uuid],
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT v.org_id, v.file_id FROM file_versions v \
         JOIN files f ON f.id = v.file_id AND f.org_id = v.org_id \
         JOIN projects p ON p.id = f.project_id \
         JOIN LATERAL (SELECT n.created_at FROM file_versions n \
                       WHERE n.file_id = v.file_id AND n.number > v.number \
                       ORDER BY n.number LIMIT 1) nx ON true \
         WHERE f.deleted_at IS NULL AND p.deleted_at IS NULL \
           AND v.id IS DISTINCT FROM f.head_version_id AND nx.created_at < $1 \
           AND v.org_id = ANY($3) AND v.file_id <> ALL($4) \
         LIMIT $2",
    )
    .bind(cutoff)
    .bind(limit)
    .bind(orgs)
    .bind(exclude)
    .fetch_all(pool)
    .await?)
}

impl OrgTx {
    /// Removes a file and everything that hangs on it: open upload sessions
    /// are closed first (reservation released, staged blob orphaned), then the
    /// versions, the sessions and the file row go. The blobs stay; they become
    /// unreferenced and the garbage collection takes them.
    async fn purge_file_rows(
        &mut self,
        file_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<PurgeCounts, DbError> {
        let open = sqlx::query_as::<_, UploadSession>(&format!(
            "SELECT {SESSION_COLUMNS} FROM upload_sessions \
             WHERE file_id = $1 AND org_id = $2 AND state IN {HOLDING} FOR UPDATE"
        ))
        .bind(file_id)
        .bind(self.org_id)
        .fetch_all(&mut *self.tx)
        .await?;
        for session in &open {
            self.close_session(session, "aborted", now).await?;
        }
        // Break the references among the rows first (head pointer, base and
        // restore lineage); the foreign keys are RESTRICT.
        sqlx::query("UPDATE files SET head_version_id = NULL WHERE id = $1 AND org_id = $2")
            .bind(file_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?;
        sqlx::query(
            "UPDATE file_versions SET base_version_id = NULL, restored_from_version_id = NULL \
             WHERE file_id = $1 AND org_id = $2",
        )
        .bind(file_id)
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?;
        let versions = sqlx::query("DELETE FROM file_versions WHERE file_id = $1 AND org_id = $2")
            .bind(file_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?
            .rows_affected();
        // An upload created for this file may have been committed into another
        // file of the workspace (the "save as copy" path). The version there
        // still references the session, so the session moves to that file
        // instead of being deleted; the foreign keys stay satisfied.
        sqlx::query(
            "UPDATE upload_sessions s SET file_id = v.file_id, project_id = v.project_id \
             FROM file_versions v \
             WHERE v.upload_session_id = s.id AND s.file_id = $1 AND s.org_id = $2 \
               AND v.file_id <> $1",
        )
        .bind(file_id)
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?;
        let sessions =
            sqlx::query("DELETE FROM upload_sessions WHERE file_id = $1 AND org_id = $2")
                .bind(file_id)
                .bind(self.org_id)
                .execute(&mut *self.tx)
                .await?
                .rows_affected();
        let files = sqlx::query("DELETE FROM files WHERE id = $1 AND org_id = $2")
            .bind(file_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?
            .rows_affected();
        Ok(PurgeCounts {
            files,
            versions,
            sessions,
            projects: 0,
        })
    }

    /// Permanently deletes a file that has been in the trash since before
    /// `cutoff`. Re-checks under the lock; a live file, a file restored in the
    /// meantime or one that is gone yields empty counts.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn maint_purge_file(
        &mut self,
        file_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<PurgeCounts, DbError> {
        let due: Option<(bool, Uuid)> = sqlx::query_as(
            "SELECT deleted_at < $3, workspace_id FROM files \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NOT NULL FOR UPDATE",
        )
        .bind(file_id)
        .bind(self.org_id)
        .bind(cutoff)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((true, workspace_id)) = due else {
            return Ok(PurgeCounts::default());
        };
        let counts = self.purge_file_rows(file_id, now).await?;
        if counts.files > 0 {
            self.audit(
                &AuditCtx::system(),
                NewEvent::new("file.purge")
                    .target("file", file_id)
                    .workspace(workspace_id)
                    .detail("versions", Detail::Count(clamp_count(counts.versions))),
            )
            .await?;
        }
        Ok(counts)
    }

    /// Permanently deletes a project that has been in the trash since before
    /// `cutoff`, together with its files (any state), at most `file_limit`
    /// files per call. The project row goes with the call that removes its
    /// last file; until then it stays in the trash and the next run continues.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn maint_purge_project(
        &mut self,
        project_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        file_limit: i64,
    ) -> Result<PurgeCounts, DbError> {
        let due: Option<(bool, Uuid)> = sqlx::query_as(
            "SELECT deleted_at < $3, workspace_id FROM projects \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NOT NULL FOR UPDATE",
        )
        .bind(project_id)
        .bind(self.org_id)
        .bind(cutoff)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((true, workspace_id)) = due else {
            return Ok(PurgeCounts::default());
        };
        let files: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM files WHERE project_id = $1 AND org_id = $2 \
             ORDER BY id LIMIT $3 FOR UPDATE",
        )
        .bind(project_id)
        .bind(self.org_id)
        .bind(file_limit)
        .fetch_all(&mut *self.tx)
        .await?;
        let mut counts = PurgeCounts::default();
        for file_id in files {
            counts.add(self.purge_file_rows(file_id, now).await?);
        }
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM files WHERE project_id = $1 AND org_id = $2")
                .bind(project_id)
                .bind(self.org_id)
                .fetch_one(&mut *self.tx)
                .await?;
        if remaining == 0 {
            counts.projects = sqlx::query("DELETE FROM projects WHERE id = $1 AND org_id = $2")
                .bind(project_id)
                .bind(self.org_id)
                .execute(&mut *self.tx)
                .await?
                .rows_affected();
        }
        // A long project is emptied over several transactions; each one that
        // removed something records what it did.
        if counts.files > 0 || counts.projects > 0 {
            self.audit(
                &AuditCtx::system(),
                NewEvent::new("project.purge")
                    .target("project", project_id)
                    .workspace(workspace_id)
                    .detail("files", Detail::Count(clamp_count(counts.files)))
                    .detail("versions", Detail::Count(clamp_count(counts.versions)))
                    .detail("completed", Detail::Flag(counts.projects == 1)),
            )
            .await?;
        }
        Ok(counts)
    }

    /// Deletes the versions of a file that are older than the retention: not
    /// the head, and superseded (by the next remaining version) before
    /// `cutoff`. Returns how many versions were deleted.
    ///
    /// A version that a remaining version names as `base_version_id` or
    /// `restored_from_version_id` is deleted anyway: the reference of the
    /// remaining version is cleared (the lineage pointer is informational, the
    /// content stays reachable through the blob that the newer version
    /// references). The committed upload sessions of the deleted versions go
    /// with them. Their blobs are untouched here; the garbage collection frees
    /// those that nothing references any more.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn maint_trim_versions(
        &mut self,
        file_id: Uuid,
        cutoff: OffsetDateTime,
    ) -> Result<u64, DbError> {
        let live: Option<(Option<Uuid>, bool, bool, Uuid)> = sqlx::query_as(
            "SELECT f.head_version_id, f.deleted_at IS NOT NULL, p.deleted_at IS NOT NULL, \
                    f.workspace_id \
             FROM files f JOIN projects p ON p.id = f.project_id \
             WHERE f.id = $1 AND f.org_id = $2 FOR UPDATE OF f",
        )
        .bind(file_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((head, false, false, workspace_id)) = live else {
            return Ok(0);
        };
        let old: Vec<Uuid> = sqlx::query_scalar(
            "SELECT v.id FROM file_versions v \
             JOIN LATERAL (SELECT n.created_at FROM file_versions n \
                           WHERE n.file_id = v.file_id AND n.number > v.number \
                           ORDER BY n.number LIMIT 1) nx ON true \
             WHERE v.file_id = $1 AND v.org_id = $2 AND v.id IS DISTINCT FROM $3 \
               AND nx.created_at < $4",
        )
        .bind(file_id)
        .bind(self.org_id)
        .bind(head)
        .bind(cutoff)
        .fetch_all(&mut *self.tx)
        .await?;
        if old.is_empty() {
            return Ok(0);
        }
        sqlx::query(
            "UPDATE file_versions SET \
                base_version_id = CASE WHEN base_version_id = ANY($2) THEN NULL ELSE base_version_id END, \
                restored_from_version_id = CASE WHEN restored_from_version_id = ANY($2) \
                    THEN NULL ELSE restored_from_version_id END \
             WHERE file_id = $1 AND org_id = $3 \
               AND (base_version_id = ANY($2) OR restored_from_version_id = ANY($2))",
        )
        .bind(file_id)
        .bind(&old)
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?;
        let sessions: Vec<Option<Uuid>> = sqlx::query_scalar(
            "DELETE FROM file_versions WHERE file_id = $1 AND org_id = $3 AND id = ANY($2) \
             RETURNING upload_session_id",
        )
        .bind(file_id)
        .bind(&old)
        .bind(self.org_id)
        .fetch_all(&mut *self.tx)
        .await?;
        let sessions: Vec<Uuid> = sessions.into_iter().flatten().collect();
        if !sessions.is_empty() {
            sqlx::query(
                "DELETE FROM upload_sessions WHERE id = ANY($1) AND org_id = $2 AND state = 'committed'",
            )
            .bind(&sessions)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?;
        }
        let deleted = old.len() as u64;
        self.audit(
            &AuditCtx::system(),
            NewEvent::new("version.trim")
                .target("file", file_id)
                .workspace(workspace_id)
                .detail("versions", Detail::Count(clamp_count(deleted))),
        )
        .await?;
        Ok(deleted)
    }
}

fn clamp_count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

// ---- blob garbage collection ------------------------------------------------

/// Blobs that nothing references and that are old enough: `(org_id, blob_id)`,
/// oldest first. State `staged` (open upload) is never a candidate.
///
/// # Errors
///
/// Database failures only.
pub async fn gc_candidates(
    pool: &PgPool,
    created_before: OffsetDateTime,
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT b.org_id, b.id FROM blobs b \
         WHERE b.state IN ('orphaned', 'committed', 'corrupt') AND b.created_at < $1 \
           AND NOT EXISTS (SELECT 1 FROM file_versions v WHERE v.blob_id = b.id) \
           AND NOT EXISTS (SELECT 1 FROM upload_sessions s \
                           WHERE s.blob_id = b.id AND s.state IN {HOLDING}) \
           AND b.id <> ALL($3) \
         ORDER BY b.created_at, b.id LIMIT $2"
    ))
    .bind(created_before)
    .bind(limit)
    .bind(exclude)
    .fetch_all(pool)
    .await?)
}

/// Blobs claimed by an earlier run whose object or row removal did not finish:
/// `(org_id, blob_id, object_key)`.
///
/// # Errors
///
/// Database failures only.
pub async fn gc_deleting(pool: &PgPool, limit: i64) -> Result<Vec<(Uuid, Uuid, String)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT org_id, id, object_key FROM blobs WHERE state = 'deleting' \
         ORDER BY created_at, id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Phase 2 of the garbage collection, after the object is gone: deletes the
/// row of a `deleting` blob. Returns whether a row was removed.
///
/// # Errors
///
/// Database failures (a foreign key violation would mean something still
/// references the blob; the row then stays `deleting`).
pub async fn gc_finish(pool: &PgPool, blob_id: Uuid) -> Result<bool, DbError> {
    let done = sqlx::query("DELETE FROM blobs WHERE id = $1 AND state = 'deleting'")
        .bind(blob_id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() == 1)
}

/// A blob claimed for deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedBlob {
    /// The object to delete from storage.
    pub object_key: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Whether the blob had been counted in `committed_bytes` (and was
    /// subtracted by the claim).
    pub counted: bool,
    /// A counter was smaller than the blob and was clamped to zero instead of
    /// going negative; usage drift correction repairs the rest.
    pub clamped: bool,
}

/// Result of [`OrgTx::maint_gc_claim`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcClaim {
    /// The blob is now `deleting`.
    Claimed(ClaimedBlob),
    /// Not claimed: referenced, too young, locked by someone else, in another
    /// state or gone.
    Skipped,
}

impl OrgTx {
    /// Subtracts committed bytes, never below zero. Returns whether a counter
    /// had to be clamped.
    async fn sub_committed(&mut self, workspace_id: Uuid, bytes: i64) -> Result<bool, DbError> {
        let ws: Option<i64> = sqlx::query_scalar(
            "SELECT committed_bytes FROM workspaces WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let org: i64 =
            sqlx::query_scalar("SELECT committed_bytes FROM organizations WHERE id = $1")
                .bind(self.org_id)
                .fetch_one(&mut *self.tx)
                .await?;
        let clamped = ws.is_none_or(|current| current < bytes) || org < bytes;
        sqlx::query(
            "UPDATE workspaces SET committed_bytes = GREATEST(committed_bytes - $3, 0) \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(workspace_id)
        .bind(self.org_id)
        .bind(bytes)
        .execute(&mut *self.tx)
        .await?;
        sqlx::query(
            "UPDATE organizations SET committed_bytes = GREATEST(committed_bytes - $2, 0) WHERE id = $1",
        )
        .bind(self.org_id)
        .bind(bytes)
        .execute(&mut *self.tx)
        .await?;
        Ok(clamped)
    }

    /// Phase 1 of the garbage collection: claims one unreferenced blob.
    ///
    /// Under the organization lock the blob row is locked (`SKIP LOCKED`),
    /// checked again (state, age, no version, no open upload session), set to
    /// `deleting`, detached from closed sessions, and subtracted from the
    /// committed counters if it was counted. The caller commits and then
    /// deletes the object and the row.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn maint_gc_claim(
        &mut self,
        blob_id: Uuid,
        created_before: OffsetDateTime,
    ) -> Result<GcClaim, DbError> {
        let row: Option<(String, i64, Uuid, String)> = sqlx::query_as(
            "SELECT object_key, size_bytes, workspace_id, state FROM blobs \
             WHERE id = $1 AND org_id = $2 AND state IN ('orphaned', 'committed', 'corrupt') \
               AND created_at < $3 FOR UPDATE SKIP LOCKED",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(created_before)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((object_key, size_bytes, workspace_id, state)) = row else {
            return Ok(GcClaim::Skipped);
        };
        let referenced: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM file_versions WHERE blob_id = $1) \
                 OR EXISTS (SELECT 1 FROM upload_sessions WHERE blob_id = $1 AND state IN {HOLDING})"
        ))
        .bind(blob_id)
        .fetch_one(&mut *self.tx)
        .await?;
        if referenced {
            return Ok(GcClaim::Skipped);
        }
        sqlx::query("UPDATE blobs SET state = 'deleting' WHERE id = $1 AND org_id = $2")
            .bind(blob_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?;
        // Closed sessions only remember the blob; let go of it so the row can go.
        sqlx::query("UPDATE upload_sessions SET blob_id = NULL WHERE blob_id = $1 AND org_id = $2")
            .bind(blob_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?;
        let counted = matches!(state.as_str(), "committed" | "corrupt");
        let clamped = if counted {
            self.sub_committed(workspace_id, size_bytes).await?
        } else {
            false
        };
        Ok(GcClaim::Claimed(ClaimedBlob {
            object_key,
            size_bytes,
            counted,
            clamped,
        }))
    }
}

// ---- reconciliation and verification ----------------------------------------

/// Which of `keys` belong to a blob row (any state).
///
/// # Errors
///
/// Database failures only.
pub async fn known_object_keys(pool: &PgPool, keys: &[String]) -> Result<HashSet<String>, DbError> {
    let found: Vec<String> =
        sqlx::query_scalar("SELECT object_key FROM blobs WHERE object_key = ANY($1)")
            .bind(keys)
            .fetch_all(pool)
            .await?;
    Ok(found.into_iter().collect())
}

/// A blob due for re-hashing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VerifyCandidate {
    /// Blob id.
    pub id: Uuid,
    /// Storage key.
    pub object_key: String,
    /// Recorded size.
    pub size_bytes: i64,
    /// Recorded SHA-256.
    pub sha256: Vec<u8>,
    /// `committed` or `corrupt`.
    pub state: String,
}

/// The committed and corrupt blobs that were verified longest ago (never
/// verified first).
///
/// # Errors
///
/// Database failures only.
pub async fn verify_candidates(pool: &PgPool, limit: i64) -> Result<Vec<VerifyCandidate>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id, object_key, size_bytes, sha256, state FROM blobs \
         WHERE state IN ('committed', 'corrupt') \
         ORDER BY verified_at NULLS FIRST, created_at, id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// What re-hashing a blob found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResult {
    /// The object matches the recorded SHA-256 (a `corrupt` blob becomes `committed` again).
    Intact,
    /// The object differs (a `committed` blob becomes `corrupt`).
    Mismatch,
    /// The object does not exist; the state is not changed.
    Missing,
}

/// Records a verification. Only blobs still `committed` or `corrupt` are
/// touched, so a concurrent garbage collection is never overwritten. Returns
/// the state after the update, or `None` if the blob is no longer a candidate.
///
/// # Errors
///
/// Database failures only.
pub async fn record_verification(
    pool: &PgPool,
    blob_id: Uuid,
    result: VerifyResult,
    now: OffsetDateTime,
) -> Result<Option<String>, DbError> {
    let sql = match result {
        VerifyResult::Intact => {
            "UPDATE blobs SET verified_at = $2, state = 'committed' \
             WHERE id = $1 AND state IN ('committed', 'corrupt') RETURNING state"
        }
        VerifyResult::Mismatch => {
            "UPDATE blobs SET verified_at = $2, state = 'corrupt' \
             WHERE id = $1 AND state IN ('committed', 'corrupt') RETURNING state"
        }
        VerifyResult::Missing => {
            "UPDATE blobs SET verified_at = $2 \
             WHERE id = $1 AND state IN ('committed', 'corrupt') RETURNING state"
        }
    };
    Ok(sqlx::query_scalar(sql)
        .bind(blob_id)
        .bind(now)
        .fetch_optional(pool)
        .await?)
}

// ---- usage drift --------------------------------------------------------------

/// Organizations among `orgs` whose usage counters (or whose workspaces'
/// counters) differ from the rows they are derived from.
///
/// # Errors
///
/// Database failures only.
pub async fn drifting_orgs(
    pool: &PgPool,
    orgs: &[Uuid],
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar(&format!(
        "WITH ws AS ( \
            SELECT w.org_id, w.reserved_bytes AS rb, w.committed_bytes AS cb, \
                COALESCE((SELECT sum(s.declared_size)::bigint FROM upload_sessions s \
                          WHERE s.workspace_id = w.id AND s.state IN {HOLDING}), 0) AS er, \
                COALESCE((SELECT sum(b.size_bytes)::bigint FROM blobs b \
                          WHERE b.workspace_id = w.id AND b.state IN ('committed', 'corrupt')), 0) AS ec \
            FROM workspaces w WHERE w.org_id = ANY($2) AND w.org_id <> ALL($3)) \
         SELECT org_id FROM ( \
            SELECT org_id FROM ws WHERE rb <> er OR cb <> ec \
            UNION \
            SELECT o.id FROM organizations o \
             WHERE o.id = ANY($2) AND o.id <> ALL($3) AND ( \
                   o.reserved_bytes <> COALESCE((SELECT sum(er)::bigint FROM ws WHERE ws.org_id = o.id), 0) \
                OR o.committed_bytes <> COALESCE((SELECT sum(ec)::bigint FROM ws WHERE ws.org_id = o.id), 0)) \
         ) d LIMIT $1"
    ))
    .bind(limit)
    .bind(orgs)
    .bind(exclude)
    .fetch_all(pool)
    .await?)
}

/// What a drift correction changed in the organization counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageDrift {
    /// New minus old `reserved_bytes` of the organization.
    pub reserved_delta: i64,
    /// New minus old `committed_bytes` of the organization.
    pub committed_delta: i64,
    /// Workspaces whose counters changed.
    pub workspaces: u64,
}

impl OrgTx {
    /// Recomputes the usage counters of the organization and its workspaces
    /// from the rows; returns what changed, `None` if they were right.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn maint_fix_usage(&mut self) -> Result<Option<UsageDrift>, DbError> {
        let before_org: (i64, i64) = sqlx::query_as(
            "SELECT reserved_bytes, committed_bytes FROM organizations WHERE id = $1",
        )
        .bind(self.org_id)
        .fetch_one(&mut *self.tx)
        .await?;
        let before_ws: Vec<(Uuid, i64, i64)> = sqlx::query_as(
            "SELECT id, reserved_bytes, committed_bytes FROM workspaces \
             WHERE org_id = $1 ORDER BY id FOR UPDATE",
        )
        .bind(self.org_id)
        .fetch_all(&mut *self.tx)
        .await?;
        self.recompute_usage().await?;
        let after_org: (i64, i64) = sqlx::query_as(
            "SELECT reserved_bytes, committed_bytes FROM organizations WHERE id = $1",
        )
        .bind(self.org_id)
        .fetch_one(&mut *self.tx)
        .await?;
        let after_ws: Vec<(Uuid, i64, i64)> = sqlx::query_as(
            "SELECT id, reserved_bytes, committed_bytes FROM workspaces \
             WHERE org_id = $1 ORDER BY id",
        )
        .bind(self.org_id)
        .fetch_all(&mut *self.tx)
        .await?;
        let workspaces = before_ws
            .iter()
            .zip(after_ws.iter())
            .filter(|(before, after)| before != after)
            .count() as u64;
        if before_org == after_org && workspaces == 0 {
            return Ok(None);
        }
        Ok(Some(UsageDrift {
            reserved_delta: after_org.0.saturating_sub(before_org.0),
            committed_delta: after_org.1.saturating_sub(before_org.1),
            workspaces,
        }))
    }
}

// ---- scheduling helpers, clock and store binding ------------------------------

/// The database clock (`now()`), used only to sanity-check the application clock.
///
/// # Errors
///
/// Database failures only.
pub async fn db_now(pool: &PgPool) -> Result<OffsetDateTime, DbError> {
    Ok(sqlx::query_scalar("SELECT now()").fetch_one(pool).await?)
}

/// The next `limit` organization ids after `after` (ascending), for steps that
/// walk the organizations in windows instead of scanning everything at once.
///
/// # Errors
///
/// Database failures only.
pub async fn org_window(
    pool: &PgPool,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM organizations WHERE ($1::uuid IS NULL OR id > $1) ORDER BY id LIMIT $2",
    )
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Organizations with open upload sessions that are past their time.
///
/// # Errors
///
/// Database failures only.
pub async fn orgs_with_due_sessions(
    pool: &PgPool,
    now: OffsetDateTime,
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT DISTINCT org_id FROM upload_sessions \
         WHERE state IN {HOLDING} AND expires_at <= $1 AND org_id <> ALL($3) LIMIT $2"
    ))
    .bind(now)
    .bind(limit)
    .bind(exclude)
    .fetch_all(pool)
    .await?)
}

/// The identity of the object store this database is bound to, if any.
///
/// # Errors
///
/// Database failures only.
pub async fn store_binding(pool: &PgPool) -> Result<Option<Uuid>, DbError> {
    Ok(sqlx::query_scalar("SELECT store_id FROM storage_binding")
        .fetch_optional(pool)
        .await?)
}

/// The identity and the epoch of the binding, if any.
///
/// # Errors
///
/// Database failures only.
pub async fn store_binding_epoch(pool: &PgPool) -> Result<Option<(Uuid, i64)>, DbError> {
    Ok(
        sqlx::query_as("SELECT store_id, epoch FROM storage_binding")
            .fetch_optional(pool)
            .await?,
    )
}

/// Raises the epoch of the binding by one and returns the new value (`None`
/// if the database is bound to another store). The marker file follows
/// afterwards; a crash in between is repaired by the next run.
///
/// # Errors
///
/// Database failures only.
pub async fn bump_binding_epoch(pool: &PgPool, store_id: Uuid) -> Result<Option<i64>, DbError> {
    Ok(sqlx::query_scalar(
        "UPDATE storage_binding SET epoch = epoch + 1 WHERE store_id = $1 RETURNING epoch",
    )
    .bind(store_id)
    .fetch_optional(pool)
    .await?)
}

/// Binds the database to a store if it has no binding yet. Returns whether
/// this call created the binding.
///
/// # Errors
///
/// Database failures only.
pub async fn bind_store_if_unbound(
    pool: &PgPool,
    store_id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let done = sqlx::query(
        "INSERT INTO storage_binding (id, store_id, bound_at) VALUES (true, $1, $2) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(store_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Sets the binding to `store_id`, replacing any earlier one (operator
/// decision, `craftcloud-server admin storage-bind --force`). Returns the old
/// identity.
///
/// # Errors
///
/// Database failures only.
pub async fn rebind_store(
    pool: &PgPool,
    store_id: Uuid,
    epoch: i64,
    now: OffsetDateTime,
) -> Result<Option<Uuid>, DbError> {
    let mut tx = pool.begin().await?;
    let old: Option<Uuid> = sqlx::query_scalar("SELECT store_id FROM storage_binding FOR UPDATE")
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO storage_binding (id, store_id, bound_at, epoch) VALUES (true, $1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET store_id = EXCLUDED.store_id, \
             bound_at = EXCLUDED.bound_at, epoch = EXCLUDED.epoch",
    )
    .bind(store_id)
    .bind(now)
    .bind(epoch)
    .execute(&mut *tx)
    .await?;
    // Operator decision from the command line: instance-level, no actor.
    crate::audit::record_in(
        &mut tx,
        &AuditCtx::system(),
        &NewEvent::new("storage.rebind")
            .target("storage_binding", store_id)
            .detail("epoch", Detail::Count(epoch))
            .detail("was_bound", Detail::Flag(old.is_some()))
            .detail("changed", Detail::Flag(old != Some(store_id))),
    )
    .await?;
    tx.commit().await?;
    Ok(old)
}

/// Removes the binding (compensation when the marker file could not be written).
///
/// # Errors
///
/// Database failures only.
pub async fn unbind_store(pool: &PgPool, store_id: Uuid) -> Result<(), DbError> {
    sqlx::query("DELETE FROM storage_binding WHERE store_id = $1")
        .bind(store_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---- run log ------------------------------------------------------------------

/// A finished run as stored.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RunRecord {
    /// Run id.
    pub id: i64,
    /// `scheduled` or `cli`.
    pub trigger: String,
    /// Start time.
    pub started_at: OffsetDateTime,
    /// End time.
    pub finished_at: OffsetDateTime,
    /// Duration in milliseconds.
    pub duration_ms: i64,
    /// The report as JSON text (counts and error codes).
    pub report: String,
}

/// Stores a finished run and keeps only the newest ones. `report` is JSON.
///
/// # Errors
///
/// Database failures only.
pub async fn record_run(
    pool: &PgPool,
    trigger: &str,
    started_at: OffsetDateTime,
    finished_at: OffsetDateTime,
    duration_ms: i64,
    report: &str,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO maintenance_runs (triggered_by, started_at, finished_at, duration_ms, report) \
         VALUES ($1, $2, $3, $4, $5::text::jsonb)",
    )
    .bind(trigger)
    .bind(started_at)
    .bind(finished_at)
    .bind(duration_ms.max(0))
    .bind(report)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM maintenance_runs WHERE id <= (SELECT max(id) FROM maintenance_runs) - $1",
    )
    .bind(KEEP_RUNS)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The newest finished run, if any.
///
/// # Errors
///
/// Database failures only.
pub async fn last_run(pool: &PgPool) -> Result<Option<RunRecord>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id, triggered_by AS trigger, started_at, finished_at, duration_ms, report::text AS report \
         FROM maintenance_runs ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?)
}
