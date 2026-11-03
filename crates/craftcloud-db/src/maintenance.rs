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
use time::{Date, OffsetDateTime};
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

/// Time-grid and count limits for file-version retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionThinningPolicy {
    /// Maximum age before the legacy version-retention rule permits deletion.
    pub retention_days: u32,
    pub keep_all_hours: u32,
    pub daily_days: u32,
    pub weekly_weeks: u32,
    /// Zero means unlimited. Pinned versions are excluded from this count.
    pub max_per_file: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct RetentionVersion {
    pub id: Uuid,
    pub number: i64,
    pub created_at: OffsetDateTime,
    pub pinned: bool,
    pub protected: bool,
}

/// IDs to retain, oldest-to-newest input accepted. Ties use the larger version
/// number, then the UUID, so results do not depend on database row order.
#[must_use]
pub fn versions_to_keep(
    versions: &[RetentionVersion],
    head: Option<Uuid>,
    policy: VersionThinningPolicy,
    now: OffsetDateTime,
) -> HashSet<Uuid> {
    let mut ordered = versions.to_vec();
    ordered.sort_by_key(|version| (version.created_at, version.number, version.id));
    let mut keep = HashSet::new();
    for version in &ordered {
        if Some(version.id) == head
            || version.pinned
            || version.protected
            || version.created_at > now
        {
            keep.insert(version.id);
        }
    }

    let grid_disabled =
        policy.keep_all_hours == 0 && policy.daily_days == 0 && policy.weekly_weeks == 0;
    if grid_disabled {
        let cutoff = now - time::Duration::days(i64::from(policy.retention_days));
        // Legacy behavior looks up the nearest greater version number, not
        // the nearest creation time. Sort once and scan distinct number runs.
        let mut by_number: Vec<_> = ordered.iter().collect();
        by_number.sort_by_key(|version| (version.number, version.created_at, version.id));
        let mut groups = by_number
            .chunk_by(|left, right| left.number == right.number)
            .peekable();
        while let Some(group) = groups.next() {
            if groups
                .peek()
                .and_then(|next| next.first())
                .is_some_and(|successor| successor.created_at >= cutoff)
            {
                keep.extend(group.iter().map(|version| version.id));
            }
        }
    }

    let daily_start = if policy.daily_days > 0 {
        now.date() - time::Duration::days(i64::from(policy.daily_days.saturating_sub(1)))
    } else {
        Date::MAX
    };
    let weekly_start = if policy.weekly_weeks > 0 {
        let (year, week, _) = now.date().to_iso_week_date();
        let monday =
            Date::from_iso_week_date(year, week, time::Weekday::Monday).unwrap_or(now.date());
        monday - time::Duration::days(i64::from(policy.weekly_weeks.saturating_sub(1)) * 7)
    } else {
        Date::MAX
    };

    let mut daily: std::collections::BTreeMap<Date, RetentionVersion> =
        std::collections::BTreeMap::new();
    let mut weekly: std::collections::BTreeMap<(i32, u8), RetentionVersion> =
        std::collections::BTreeMap::new();
    for version in &ordered {
        if keep.contains(&version.id)
            || version.created_at > now
            || now - version.created_at > time::Duration::days(i64::from(policy.retention_days))
        {
            continue;
        }
        let age = now - version.created_at;
        if policy.keep_all_hours > 0
            && age < time::Duration::hours(i64::from(policy.keep_all_hours))
        {
            keep.insert(version.id);
        }
        let date = version.created_at.date();
        if policy.daily_days > 0 && date >= daily_start && date <= now.date() {
            daily.insert(date, *version);
        }
        if policy.weekly_weeks > 0 && date >= weekly_start && date <= now.date() {
            let (year, week, _) = date.to_iso_week_date();
            weekly.insert((year, week), *version);
        }
    }
    keep.extend(daily.values().map(|version| version.id));
    keep.extend(weekly.values().map(|version| version.id));

    if policy.max_per_file > 0 {
        let mut excess = ordered
            .iter()
            .filter(|version| keep.contains(&version.id) && !version.pinned)
            .count()
            .saturating_sub(policy.max_per_file as usize);
        for version in &ordered {
            if excess == 0 {
                break;
            }
            if keep.contains(&version.id)
                && !version.pinned
                && !version.protected
                && Some(version.id) != head
                && version.created_at <= now
            {
                keep.remove(&version.id);
                excess -= 1;
            }
        }
    }
    keep
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

/// Files with more than one version that can be evaluated by version thinning.
pub async fn files_with_versions_to_thin(
    pool: &PgPool,
    after: Option<(Uuid, Uuid)>,
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT f.org_id, f.id FROM files f JOIN projects p ON p.id=f.project_id \
         WHERE f.deleted_at IS NULL AND p.deleted_at IS NULL \
           AND ($1::uuid IS NULL OR (f.org_id, f.id) > ($1, $2)) \
           AND f.id<>ALL($3) AND (SELECT count(*) FROM file_versions v WHERE v.file_id=f.id)>1 \
         ORDER BY f.org_id, f.id LIMIT $4",
    )
    .bind(after.map(|(org_id, _)| org_id))
    .bind(after.map(|(_, file_id)| file_id))
    .bind(exclude)
    .bind(limit)
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
        let live_busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM figcraft_live_files l WHERE l.file_id=$1 AND (l.state<>'republish' OR l.figcraft_file_id IS NOT NULL OR EXISTS(SELECT 1 FROM figcraft_links k WHERE k.file_id=l.file_id AND k.revoked_at IS NULL) OR EXISTS(SELECT 1 FROM figcraft_outbox o WHERE o.file_id=l.file_id)))")
            .bind(file_id).fetch_one(&mut *self.tx).await?;
        if live_busy {
            return Ok(PurgeCounts::default());
        }
        sqlx::query("DELETE FROM file_locks WHERE file_id = $1 AND org_id = $2")
            .bind(file_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?;
        sqlx::query("DELETE FROM grants WHERE org_id = $1 AND file_id = $2")
            .bind(self.org_id)
            .bind(file_id)
            .execute(&mut *self.tx)
            .await?;
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
        sqlx::query("DELETE FROM figcraft_links WHERE file_id=$1")
            .bind(file_id)
            .execute(&mut *self.tx)
            .await?;
        sqlx::query("DELETE FROM figcraft_live_files WHERE file_id=$1 AND state='republish' AND figcraft_file_id IS NULL")
            .bind(file_id).execute(&mut *self.tx).await?;
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
            sqlx::query("DELETE FROM grants WHERE org_id = $1 AND project_id = $2")
                .bind(self.org_id)
                .bind(project_id)
                .execute(&mut *self.tx)
                .await?;
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
        policy: VersionThinningPolicy,
        now: OffsetDateTime,
        dry_run: bool,
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
        let versions: Vec<RetentionVersion> = sqlx::query_as(
            "SELECT v.id, v.number, v.created_at, v.pinned, \
                (EXISTS (SELECT 1 FROM figcraft_live_files l WHERE l.file_id=v.file_id \
                         AND (l.published_from_version_id=v.id OR l.last_snapshot_version_id=v.id)) \
                 OR EXISTS (SELECT 1 FROM upload_sessions s WHERE s.file_id=v.file_id \
                            AND s.org_id=v.org_id AND s.state IN ('created','receiving','stored') \
                            AND s.expires_at > $3)) AS protected \
             FROM file_versions v WHERE v.file_id=$1 AND v.org_id=$2 ORDER BY v.created_at, v.number, v.id",
        )
        .bind(file_id)
        .bind(self.org_id)
        .bind(now)
        .fetch_all(&mut *self.tx)
        .await?;
        let keep = versions_to_keep(&versions, head, policy, now);
        let old = versions
            .iter()
            .filter(|version| !keep.contains(&version.id))
            .map(|version| version.id)
            .collect::<Vec<_>>();
        if old.is_empty() {
            return Ok(0);
        }
        if dry_run {
            return Ok(old.len() as u64);
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

/// A blob claimed for archive publication. `claim_token` fences stale workers;
/// callers must retain it through publication and local-object cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveClaim {
    /// Stable object identity used as encryption AAD.
    pub blob_id: Uuid,
    /// Local object key.
    pub object_key: String,
    /// Existing archive key, if this blob was recalled before.
    pub archive_object_key: Option<String>,
    /// Wrapped per-object data key, if an archive copy already exists.
    pub archive_wrapped_key: Option<Vec<u8>>,
    /// Size in bytes.
    pub size_bytes: i64,
    /// SHA-256 of the plaintext.
    pub sha256: Vec<u8>,
    /// Fencing token supplied by the caller.
    pub claim_token: Uuid,
}

type ArchiveClaimRow = (String, Option<String>, Option<Vec<u8>>, i64, Vec<u8>);
type ChunkArchiveClaimRow = (
    String,
    Option<String>,
    Option<Vec<u8>>,
    i64,
    Vec<u8>,
    i64,
    String,
);
type ArchiveRecallRow = (
    String,
    Option<String>,
    Option<Vec<u8>>,
    i64,
    Vec<u8>,
    Option<OffsetDateTime>,
);

/// Archived blob claimed for safe return to the local store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRecallClaim {
    /// Stable object identity used as encryption AAD.
    pub blob_id: Uuid,
    /// Local destination key.
    pub object_key: String,
    /// Archive source key.
    pub archive_object_key: String,
    /// Wrapped per-object data key.
    pub archive_wrapped_key: Vec<u8>,
    /// Size in bytes.
    pub size_bytes: i64,
    /// SHA-256 of the plaintext.
    pub sha256: Vec<u8>,
    /// Fencing token supplied by the caller.
    pub claim_token: Uuid,
}

/// Chunk claimed for encrypted archive publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkArchiveClaim {
    /// Stable chunk identity used as archive encryption AAD.
    pub chunk_id: Uuid,
    /// Local source object key.
    pub object_key: String,
    /// Persisted archive key from a previous attempt.
    pub archive_object_key: Option<String>,
    /// Persisted wrapped data key from a previous attempt.
    pub archive_wrapped_key: Option<Vec<u8>>,
    /// Plaintext size.
    pub size_bytes: i64,
    /// Plaintext SHA-256.
    pub sha256: Vec<u8>,
    /// Physical stored length.
    pub stored_bytes: i64,
    /// Physical encoding.
    pub encoding: String,
    /// Claim token fencing other workers.
    pub claim_token: Uuid,
}

/// Result of finalizing a verified archive copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFinish {
    /// Blob is archived; local deletion may proceed while holding `OrgTx`.
    Archived,
    /// A version became head or the claim was superseded. Keep local bytes.
    KeepLocal,
    /// Claim no longer belongs to this worker.
    StaleClaim,
}

pub(crate) const ARCHIVE_DUE: &str = "b.layout = 'whole' AND NOT EXISTS ( \
    SELECT 1 FROM file_versions v \
    JOIN files f ON f.id = v.file_id AND f.org_id = v.org_id \
    LEFT JOIN LATERAL ( \
        SELECT n.created_at FROM file_versions n \
        WHERE n.file_id = v.file_id AND n.org_id = v.org_id AND n.number > v.number \
        ORDER BY n.number LIMIT 1 \
    ) nx ON true \
    WHERE v.blob_id = b.id AND v.org_id = b.org_id \
      AND (f.head_version_id = v.id \
           OR nx.created_at IS NULL OR nx.created_at >= $1) \
) AND EXISTS (SELECT 1 FROM file_versions v WHERE v.blob_id = b.id AND v.org_id = b.org_id)";

/// Due, non-head blobs for archive work. The age boundary uses each version's
/// actual superseding version; all references must be old enough. A blob with
/// unknown lineage or any head reference stays local, including trash heads.
pub async fn archive_candidates(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
    orgs: &[Uuid],
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT b.org_id, b.id FROM blobs b \
         WHERE b.layout = 'whole' AND b.state = 'committed' AND b.tier_state IN ('local', 'archiving') \
           AND (b.archive_next_attempt_at IS NULL OR b.archive_next_attempt_at <= $2) \
           AND b.org_id = ANY($3) AND b.id <> ALL($4) AND {ARCHIVE_DUE} \
         ORDER BY b.created_at, b.id LIMIT $5"
    ))
    .bind(cutoff)
    .bind(now)
    .bind(orgs)
    .bind(exclude)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

const CHUNK_ARCHIVE_DUE: &str = "NOT EXISTS ( \
    SELECT 1 FROM version_chunks vc \
    JOIN file_versions v ON v.id = vc.version_id AND v.file_id = vc.file_id \
    JOIN files f ON f.id = v.file_id AND f.org_id = v.org_id \
    LEFT JOIN LATERAL (SELECT n.created_at FROM file_versions n \
        WHERE n.file_id = v.file_id AND n.org_id = v.org_id AND n.number > v.number \
        ORDER BY n.number LIMIT 1) nx ON true \
    WHERE vc.chunk_id = c.id AND v.org_id = c.org_id \
      AND (f.head_version_id = v.id OR nx.created_at IS NULL OR nx.created_at >= $1) \
) AND EXISTS (SELECT 1 FROM version_chunks vc WHERE vc.chunk_id = c.id) \
  AND NOT EXISTS (SELECT 1 FROM upload_chunks u WHERE u.chunk_id = c.id)";

/// Chunks whose every version reference is superseded past the archive delay and no upload is using them.
pub async fn archive_chunk_candidates(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
    stale_before: OffsetDateTime,
    orgs: &[Uuid],
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT c.org_id, c.id FROM chunks c \
         WHERE c.state = 'committed' AND c.storage_tier IN ('local', 'archiving') \
           AND (c.archive_next_attempt_at IS NULL OR c.archive_next_attempt_at <= $2) \
           AND (c.storage_tier = 'local' OR c.archive_claimed_at <= $3) \
           AND c.org_id = ANY($4) AND {CHUNK_ARCHIVE_DUE} \
         ORDER BY c.created_at, c.id LIMIT $5"
    ))
    .bind(cutoff)
    .bind(now)
    .bind(stale_before)
    .bind(orgs)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Local cleanup interrupted after a verified chunk archive copy committed.
pub async fn pending_archive_chunk_local_deletes(
    pool: &PgPool,
    now: OffsetDateTime,
    limit: i64,
) -> Result<Vec<(Uuid, Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT org_id, id, archive_claim_token FROM chunks WHERE storage_tier='archive' \
         AND archive_local_delete_pending AND archive_claim_token IS NOT NULL \
         AND (archive_next_attempt_at IS NULL OR archive_next_attempt_at <= $1) \
         ORDER BY archive_next_attempt_at NULLS FIRST, archive_claimed_at, id LIMIT $2",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Finds archived blobs whose local-copy cleanup was interrupted. Callers
/// must hold the global maintenance lock and recheck each token under `OrgTx`.
pub async fn pending_archive_local_deletes(
    pool: &PgPool,
    now: OffsetDateTime,
    limit: i64,
) -> Result<Vec<(Uuid, Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT org_id, id, archive_claim_token FROM blobs \
         WHERE tier_state = 'archived' AND archive_local_delete_pending \
           AND archive_claim_token IS NOT NULL \
           AND (archive_next_attempt_at IS NULL OR archive_next_attempt_at <= $1) \
         ORDER BY archive_next_attempt_at NULLS FIRST, archive_claimed_at, id LIMIT $2",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Readable archive health summary for operator surfaces. Contains counts and
/// bounded error codes only; no tenant names, object keys or hashes.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ArchiveStatus {
    /// Bytes with a confirmed or pending local copy.
    pub local_bytes: i64,
    /// Bytes with a referenced archive object.
    pub archive_bytes: i64,
    /// Committed blobs not yet fully archived.
    pub pending_archives: i64,
    /// Blobs with a recorded archive publication or recall error.
    pub archive_errors: i64,
    /// Archive object deletions awaiting retry or completion.
    pub pending_archive_deletes: i64,
    /// Archive object deletions with a recorded error.
    pub archive_delete_errors: i64,
    /// Most recent successful archive verification.
    pub last_verified_at: Option<OffsetDateTime>,
    /// Most recent bounded archive or delete error code.
    pub last_error_code: Option<String>,
}

/// Whether any blob is outside the local-only state. Zero-byte blobs and
/// rows marked `deleting` still require archive configuration for recovery.
pub async fn has_nonlocal_archive(pool: &PgPool) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM blobs WHERE tier_state <> 'local')")
            .fetch_one(pool)
            .await?,
    )
}

/// Archive object retained by a durable backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveBackupRef {
    /// Blob identity, retained even if its row is later removed.
    pub blob_id: Uuid,
    /// Opaque archive provider key.
    pub archive_object_key: String,
}

/// Archived chunk retained by a durable backup.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ArchiveChunkBackupRef {
    /// Chunk identity, retained even if its row is later removed.
    pub chunk_id: Uuid,
    /// Opaque archive provider key.
    pub archive_object_key: String,
}

/// Whether the operator completed recovery-backup inventory after install or restore.
pub async fn archive_gc_inventory_complete(pool: &PgPool) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE((SELECT inventory_complete FROM archive_gc_state WHERE id), false)",
    )
    .fetch_one(pool)
    .await?)
}

/// Closes archive GC before an operator begins external backup inventory.
pub async fn archive_gc_mark_inventory_incomplete(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE archive_gc_state SET inventory_complete = false, updated_at = now() WHERE id = true",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Registers archive references before a backup manifest is published.
/// Existing pins survive process crashes, intentionally preferring retention over loss.
pub async fn archive_backup_begin(pool: &PgPool, backup_id: Uuid) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO archive_backups (backup_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(backup_id)
        .execute(&mut *tx)
        .await?;
    let _digest: Option<String> = sqlx::query_scalar(
        "SELECT manifest_sha256 FROM archive_backups WHERE backup_id = $1 FOR UPDATE",
    )
    .bind(backup_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Adds one bounded batch of pins to an incomplete backup registration.
pub async fn archive_backup_add_refs(
    pool: &PgPool,
    backup_id: Uuid,
    refs: &[ArchiveBackupRef],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let digest: Option<String> = sqlx::query_scalar(
        "SELECT manifest_sha256 FROM archive_backups WHERE backup_id = $1 FOR UPDATE",
    )
    .bind(backup_id)
    .fetch_one(&mut *tx)
    .await?;
    for reference in refs {
        if digest.is_none() {
            sqlx::query(
                "INSERT INTO archive_backup_refs (backup_id, blob_id, archive_object_key) \
                 VALUES ($1, $2, $3) ON CONFLICT (backup_id, blob_id) DO NOTHING",
            )
            .bind(backup_id)
            .bind(reference.blob_id)
            .bind(&reference.archive_object_key)
            .execute(&mut *tx)
            .await?;
        }
        let stored: String = sqlx::query_scalar(
            "SELECT archive_object_key FROM archive_backup_refs WHERE backup_id = $1 AND blob_id = $2",
        )
        .bind(backup_id)
        .bind(reference.blob_id)
        .fetch_one(&mut *tx)
        .await?;
        if stored != reference.archive_object_key {
            tx.rollback().await?;
            return Err(DbError::Internal);
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Adds one bounded batch of archived-chunk pins to a backup registration.
pub async fn archive_backup_add_chunk_refs(
    pool: &PgPool,
    backup_id: Uuid,
    refs: &[ArchiveChunkBackupRef],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let digest: Option<String> = sqlx::query_scalar(
        "SELECT manifest_sha256 FROM archive_backups WHERE backup_id = $1 FOR UPDATE",
    )
    .bind(backup_id)
    .fetch_one(&mut *tx)
    .await?;
    for reference in refs {
        if digest.is_none() {
            sqlx::query(
                "INSERT INTO archive_chunk_backup_refs (backup_id, chunk_id, archive_object_key) \
                 VALUES ($1, $2, $3) ON CONFLICT (backup_id, chunk_id) DO NOTHING",
            )
            .bind(backup_id)
            .bind(reference.chunk_id)
            .bind(&reference.archive_object_key)
            .execute(&mut *tx)
            .await?;
        }
        let stored: String = sqlx::query_scalar(
            "SELECT archive_object_key FROM archive_chunk_backup_refs WHERE backup_id = $1 AND chunk_id = $2",
        )
        .bind(backup_id)
        .bind(reference.chunk_id)
        .fetch_one(&mut *tx)
        .await?;
        if stored != reference.archive_object_key {
            tx.rollback().await?;
            return Err(DbError::Internal);
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Lists pinned chunk objects for exact manifest comparison during backup verification and restore.
pub async fn archive_backup_chunk_refs(
    pool: &PgPool,
    backup_id: Uuid,
) -> Result<Vec<ArchiveChunkBackupRef>, DbError> {
    Ok(sqlx::query_as(
        "SELECT chunk_id, archive_object_key FROM archive_chunk_backup_refs WHERE backup_id = $1 ORDER BY chunk_id",
    )
    .bind(backup_id)
    .fetch_all(pool)
    .await?)
}

/// Finalizes a registered backup after its manifest and checksum are durable.
pub async fn archive_backup_complete(
    pool: &PgPool,
    backup_id: Uuid,
    manifest_sha256: &str,
    expected_refs: u64,
) -> Result<bool, DbError> {
    let expected_refs = i64::try_from(expected_refs).unwrap_or(i64::MAX);
    let done = sqlx::query(
        "UPDATE archive_backups SET manifest_sha256 = $2 \
         WHERE backup_id = $1 AND manifest_sha256 IS NULL \
           AND ((SELECT count(*) FROM archive_backup_refs WHERE backup_id = $1) + \
                (SELECT count(*) FROM archive_chunk_backup_refs WHERE backup_id = $1)) = $3",
    )
    .bind(backup_id)
    .bind(manifest_sha256)
    .bind(expected_refs)
    .execute(pool)
    .await?;
    if done.rows_affected() == 1 {
        return Ok(true);
    }
    let same: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM archive_backups WHERE backup_id = $1 AND manifest_sha256 = $2) \
         AND ((SELECT count(*) FROM archive_backup_refs WHERE backup_id = $1) + \
              (SELECT count(*) FROM archive_chunk_backup_refs WHERE backup_id = $1)) = $3",
    )
    .bind(backup_id)
    .bind(manifest_sha256)
    .bind(expected_refs)
    .fetch_one(pool)
    .await?;
    Ok(same)
}

/// Removes only an incomplete backup registration after its partial directory was removed.
pub async fn archive_backup_cancel(pool: &PgPool, backup_id: Uuid) -> Result<bool, DbError> {
    let done =
        sqlx::query("DELETE FROM archive_backups WHERE backup_id = $1 AND manifest_sha256 IS NULL")
            .bind(backup_id)
            .execute(pool)
            .await?;
    Ok(done.rows_affected() == 1)
}

/// Imports a complete backup's pins. Re-import is idempotent only when digest
/// and every reference match exactly.
pub async fn archive_backup_import(
    pool: &PgPool,
    backup_id: Uuid,
    manifest_sha256: &str,
    refs: &[ArchiveBackupRef],
) -> Result<bool, DbError> {
    archive_backup_begin(pool, backup_id).await?;
    archive_backup_add_refs(pool, backup_id, refs).await?;
    let count = u64::try_from(refs.len()).map_err(|_| DbError::Internal)?;
    archive_backup_complete(pool, backup_id, manifest_sha256, count).await
}

/// Releases backup pins only when id and manifest digest both match. The
/// caller must hold the maintenance lock and require explicit operator confirmation.
pub async fn archive_backup_release(
    pool: &PgPool,
    backup_id: Uuid,
    manifest_sha256: &str,
) -> Result<Option<u64>, DbError> {
    let mut tx = pool.begin().await?;
    let refs: i64 =
        sqlx::query_scalar("SELECT (SELECT count(*) FROM archive_backup_refs WHERE backup_id = $1) + (SELECT count(*) FROM archive_chunk_backup_refs WHERE backup_id = $1)")
            .bind(backup_id)
            .fetch_one(&mut *tx)
            .await?;
    let done =
        sqlx::query("DELETE FROM archive_backups WHERE backup_id = $1 AND manifest_sha256 = $2")
            .bind(backup_id)
            .bind(manifest_sha256)
            .execute(&mut *tx)
            .await?;
    if done.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(None);
    }
    crate::audit::record_in(
        &mut tx,
        &AuditCtx::system(),
        &NewEvent::new("backup.archive_refs_released")
            .target("archive_backup", backup_id)
            .detail("references", Detail::Count(refs)),
    )
    .await?;
    tx.commit().await?;
    Ok(Some(u64::try_from(refs).unwrap_or(0)))
}

/// Removes pins created by a restore that has not committed its database
/// payload. This compensation is bound to the exact id, manifest digest and
/// reference count and deliberately emits no audit row: the empty target must
/// remain empty so a failed restore can be retried. Operator initiated release
/// must use [`archive_backup_release`] and is always audited.
pub async fn archive_backup_abort_restore(
    pool: &PgPool,
    backup_id: Uuid,
    manifest_sha256: &str,
    expected_refs: u64,
) -> Result<bool, DbError> {
    let expected_refs = i64::try_from(expected_refs).map_err(|_| DbError::Internal)?;
    let mut tx = pool.begin().await?;
    let refs: i64 =
        sqlx::query_scalar("SELECT (SELECT count(*) FROM archive_backup_refs WHERE backup_id = $1) + (SELECT count(*) FROM archive_chunk_backup_refs WHERE backup_id = $1)")
            .bind(backup_id)
            .fetch_one(&mut *tx)
            .await?;
    if refs != expected_refs {
        tx.rollback().await?;
        return Ok(false);
    }
    let done =
        sqlx::query("DELETE FROM archive_backups WHERE backup_id = $1 AND manifest_sha256 = $2")
            .bind(backup_id)
            .bind(manifest_sha256)
            .execute(&mut *tx)
            .await?;
    if done.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// Opens archive GC only after the operator confirms that every retained backup
/// was inventoried and incomplete crash registrations were reconciled.
pub async fn archive_gc_mark_inventory_complete(
    pool: &PgPool,
    operator_confirmed_complete: bool,
) -> Result<bool, DbError> {
    if !operator_confirmed_complete {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM archive_backups WHERE manifest_sha256 IS NULL)",
    )
    .fetch_one(&mut *tx)
    .await?;
    if pending {
        tx.rollback().await?;
        return Ok(false);
    }
    let done = sqlx::query(
        "UPDATE archive_gc_state SET inventory_complete = true, updated_at = now() WHERE id",
    )
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    crate::audit::record_in(
        &mut tx,
        &AuditCtx::system(),
        &NewEvent::new("backup.archive_inventory_completed"),
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// A retired remote archive object eligible for cleanup after the stale-writer grace.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RetiredArchiveObject {
    /// Tenant that originally owned the object.
    pub org_id: Uuid,
    /// Provider key; never derived from user input.
    pub archive_object_key: String,
    /// Stored physical bytes retained for operator accounting.
    pub physical_bytes: i64,
}

const ARCHIVE_OBJECT_REFERENCED: &str = "EXISTS (SELECT 1 FROM blobs WHERE archive_object_key={key}) \
    OR EXISTS (SELECT 1 FROM chunks WHERE archive_object_key={key}) \
    OR EXISTS (SELECT 1 FROM archive_backup_refs WHERE archive_object_key={key}) \
    OR EXISTS (SELECT 1 FROM archive_chunk_backup_refs WHERE archive_object_key={key})";

/// Lists unreferenced archive keys only after inventory is complete and both
/// the stale-writer grace and retry delay have elapsed.
pub async fn retired_archive_objects_due(
    pool: &PgPool,
    stale_before: OffsetDateTime,
    now: OffsetDateTime,
    limit: i64,
) -> Result<Vec<RetiredArchiveObject>, DbError> {
    let references = ARCHIVE_OBJECT_REFERENCED.replace("{key}", "r.archive_object_key");
    Ok(sqlx::query_as(&format!(
        "SELECT r.org_id, r.archive_object_key, r.physical_bytes \
         FROM retired_archive_objects r CROSS JOIN archive_gc_state s \
         WHERE s.id AND s.inventory_complete AND r.retired_at <= $1 AND r.next_attempt_at <= $2 \
           AND NOT ({references}) \
         ORDER BY r.next_attempt_at, r.retired_at, r.archive_object_key LIMIT $3"
    ))
    .bind(stale_before)
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Records a retry without dropping durable cleanup ownership.
pub async fn retired_archive_object_failed(
    pool: &PgPool,
    archive_object_key: &str,
    retry_at: OffsetDateTime,
    code: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        "UPDATE retired_archive_objects SET attempts=attempts+1, next_attempt_at=$2, last_error_code=$3 \
         WHERE archive_object_key=$1",
    )
    .bind(archive_object_key)
    .bind(retry_at)
    .bind(code)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Drops the tombstone only after the provider delete succeeded (or returned
/// NotFound) and the object remains unreferenced.
pub async fn retired_archive_object_finish(
    pool: &PgPool,
    archive_object_key: &str,
) -> Result<bool, DbError> {
    let references = ARCHIVE_OBJECT_REFERENCED.replace("{key}", "$1");
    let result = sqlx::query(&format!(
        "DELETE FROM retired_archive_objects WHERE archive_object_key=$1 AND NOT ({references})"
    ))
    .bind(archive_object_key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Reads archive health counters for the instance operator view.
pub async fn archive_status(
    pool: &PgPool,
    archive_after: std::time::Duration,
) -> Result<ArchiveStatus, DbError> {
    let now = OffsetDateTime::now_utc();
    let seconds = i64::try_from(archive_after.as_secs()).unwrap_or(i64::MAX);
    let cutoff = now
        .checked_sub(time::Duration::seconds(seconds))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let query = format!(
        "SELECT \
           (COALESCE(sum(size_bytes) FILTER (WHERE layout = 'whole' AND state <> 'deleting' AND \
             (tier_state IN ('local', 'archiving', 'recalling') OR archive_local_delete_pending)), 0) + \
             COALESCE((SELECT sum(stored_bytes) FROM chunks WHERE state <> 'deleting' AND (storage_tier IN ('local','archiving') OR archive_local_delete_pending)), 0))::bigint AS local_bytes, \
           (COALESCE(sum(size_bytes) FILTER (WHERE layout = 'whole' AND state <> 'deleting' AND archive_object_key IS NOT NULL \
             AND archive_verified_at IS NOT NULL AND tier_state IN ('local', 'archiving', 'archived', 'recalling')), 0) + \
             COALESCE((SELECT sum(stored_bytes) FROM chunks WHERE state <> 'deleting' AND archive_object_key IS NOT NULL AND archive_verified_at IS NOT NULL AND storage_tier IN ('archiving','archive')), 0) + \
             COALESCE((SELECT sum(r.physical_bytes) FROM retired_archive_objects r WHERE \
               NOT EXISTS (SELECT 1 FROM blobs b WHERE b.archive_object_key=r.archive_object_key) AND \
               NOT EXISTS (SELECT 1 FROM chunks c WHERE c.archive_object_key=r.archive_object_key)), 0))::bigint AS archive_bytes, \
           count(*) FILTER (WHERE layout = 'whole' AND state = 'committed' AND tier_state IN ('local', 'archiving') \
             AND (archive_next_attempt_at IS NULL OR archive_next_attempt_at <= $2) AND {ARCHIVE_DUE}) + \
           (SELECT count(*) FROM chunks c WHERE c.state = 'committed' AND c.storage_tier IN ('local','archiving') \
             AND (c.archive_next_attempt_at IS NULL OR c.archive_next_attempt_at <= $2) AND {CHUNK_ARCHIVE_DUE})::bigint AS pending_archives, \
           (count(*) FILTER (WHERE archive_error_code IS NOT NULL) + (SELECT count(*) FROM chunks WHERE archive_error_code IS NOT NULL))::bigint AS archive_errors, \
           (count(*) FILTER (WHERE archive_local_delete_pending OR (state = 'deleting' AND archive_delete_pending)) + \
             (SELECT count(*) FROM chunks WHERE archive_local_delete_pending))::bigint AS pending_archive_deletes, \
           (count(*) FILTER (WHERE archive_local_delete_error_code IS NOT NULL OR archive_delete_error_code IS NOT NULL) + \
             (SELECT count(*) FROM chunks WHERE archive_error_code IS NOT NULL AND archive_local_delete_pending))::bigint AS archive_delete_errors, \
           GREATEST(max(archive_verified_at), (SELECT max(archive_verified_at) FROM chunks)) AS last_verified_at, \
           (SELECT error_code FROM ( \
              SELECT archive_error_code AS error_code, archive_error_at AS error_at FROM blobs WHERE archive_error_code IS NOT NULL \
              UNION ALL SELECT archive_local_delete_error_code, archive_local_delete_error_at FROM blobs WHERE archive_local_delete_error_code IS NOT NULL \
              UNION ALL SELECT archive_delete_error_code, archive_delete_error_at FROM blobs WHERE archive_delete_error_code IS NOT NULL \
              UNION ALL SELECT archive_error_code, archive_verified_at FROM chunks WHERE archive_error_code IS NOT NULL \
           ) e ORDER BY error_at DESC NULLS LAST LIMIT 1) AS last_error_code \
         FROM blobs b"
    );
    Ok(sqlx::query_as(&query)
        .bind(cutoff)
        .bind(now)
        .fetch_one(pool)
        .await?)
}

impl OrgTx {
    /// Claims a due blob. A stale `archiving` claim may be replaced; the token
    /// prevents its old worker from committing or deleting local data.
    pub async fn maint_archive_claim(
        &mut self,
        blob_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        stale_before: OffsetDateTime,
        claim_token: Uuid,
    ) -> Result<Option<ArchiveClaim>, DbError> {
        let due = ARCHIVE_DUE.replace("$1", "$5");
        let row: Option<ArchiveClaimRow> = sqlx::query_as(&format!(
            "SELECT b.object_key, b.archive_object_key, b.archive_wrapped_key, \
                        b.size_bytes, b.sha256 \
                 FROM blobs b WHERE b.id = $1 AND b.org_id = $2 AND b.layout = 'whole' AND b.state = 'committed' \
                   AND (b.archive_next_attempt_at IS NULL OR b.archive_next_attempt_at <= $3) \
                   AND (b.tier_state = 'local' OR \
                        (b.tier_state = 'archiving' AND b.archive_claimed_at <= $4)) \
                   AND {due} FOR UPDATE OF b SKIP LOCKED"
        ))
        .bind(blob_id)
        .bind(self.org_id)
        .bind(now)
        .bind(stale_before)
        .bind(cutoff)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((object_key, archive_object_key, archive_wrapped_key, size_bytes, sha256)) = row
        else {
            return Ok(None);
        };
        let claimed = sqlx::query(
            "UPDATE blobs SET tier_state = 'archiving', archive_claim_token = $3, \
                 archive_claimed_at = $4, archive_attempts = archive_attempts + 1, \
                 archive_error_code = NULL, archive_error_at = NULL, archive_next_attempt_at = NULL \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .bind(now)
        .execute(&mut *self.tx)
        .await?;
        if claimed.rows_affected() != 1 {
            return Ok(None);
        }
        Ok(Some(ArchiveClaim {
            blob_id,
            object_key,
            archive_object_key,
            archive_wrapped_key,
            size_bytes,
            sha256,
            claim_token,
        }))
    }

    /// Commits a verified archive object. Rechecks the claim and head status
    /// under the organization and blob locks. The claim remains until local
    /// deletion completes.
    pub async fn maint_archive_finish(
        &mut self,
        blob_id: Uuid,
        claim_token: Uuid,
        archive_object_key: &str,
        archive_wrapped_key: &[u8],
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<ArchiveFinish, DbError> {
        let row: Option<(String, Option<Uuid>)> = sqlx::query_as(
            "SELECT tier_state, archive_claim_token FROM blobs \
             WHERE id = $1 AND org_id = $2 AND state = 'committed' FOR UPDATE",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((tier_state, token)) = row else {
            return Ok(ArchiveFinish::StaleClaim);
        };
        if tier_state != "archiving" || token != Some(claim_token) {
            return Ok(ArchiveFinish::StaleClaim);
        }
        let due: bool = sqlx::query_scalar(&format!(
            "SELECT {ARCHIVE_DUE} FROM blobs b WHERE b.id = $2 AND b.org_id = $3"
        ))
        .bind(cutoff)
        .bind(blob_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?
        .unwrap_or(false);
        if !due {
            sqlx::query(
                "UPDATE blobs SET tier_state = 'local', archive_claim_token = NULL, \
                    archive_claimed_at = NULL, archive_object_key = $3, \
                    archive_wrapped_key = $4, archive_verified_at = $5 \
                 WHERE id = $1 AND org_id = $2",
            )
            .bind(blob_id)
            .bind(self.org_id)
            .bind(archive_object_key)
            .bind(archive_wrapped_key)
            .bind(now)
            .execute(&mut *self.tx)
            .await?;
            return Ok(ArchiveFinish::KeepLocal);
        }
        sqlx::query(
            "UPDATE blobs SET tier_state = 'archived', archive_object_key = $3, \
                 archive_wrapped_key = $4, archive_verified_at = $5, \
                 archive_local_delete_pending = true \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(archive_object_key)
        .bind(archive_wrapped_key)
        .bind(now)
        .execute(&mut *self.tx)
        .await?;
        Ok(ArchiveFinish::Archived)
    }

    /// Returns local deletion key while holding organization and blob locks.
    /// Keep this transaction open through the storage delete and finish call.
    pub async fn maint_archive_local_delete_target(
        &mut self,
        blob_id: Uuid,
        claim_token: Uuid,
    ) -> Result<Option<String>, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT b.object_key FROM blobs b \
             WHERE b.id = $1 AND b.org_id = $2 AND b.state = 'committed' \
               AND b.tier_state = 'archived' AND b.archive_claim_token = $3 \
               AND NOT EXISTS (SELECT 1 FROM file_versions v JOIN files f \
                    ON f.id = v.file_id AND f.org_id = v.org_id \
                    WHERE v.blob_id = b.id AND v.org_id = b.org_id AND f.head_version_id = v.id) \
             FOR UPDATE OF b",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    /// Clears the archive claim after the local object was deleted successfully.
    pub async fn maint_archive_local_delete_finish(
        &mut self,
        blob_id: Uuid,
        claim_token: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "UPDATE blobs SET archive_claim_token = NULL, archive_claimed_at = NULL, \
                 archive_local_delete_error_code = NULL, archive_local_delete_error_at = NULL, \
                 archive_verified_at = $4, archive_local_delete_pending = false, \
                 archive_next_attempt_at = NULL \
             WHERE id = $1 AND org_id = $2 AND tier_state = 'archived' \
               AND archive_claim_token = $3 \
               AND NOT EXISTS (SELECT 1 FROM file_versions v JOIN files f \
                    ON f.id = v.file_id AND f.org_id = v.org_id \
                    WHERE v.blob_id = blobs.id AND v.org_id = blobs.org_id \
                      AND f.head_version_id = v.id)",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .bind(now)
        .execute(&mut *self.tx)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Persists a bounded error code after local cleanup failed. The fenced
    /// claim remains so maintenance can retry after restart.
    pub async fn maint_archive_local_delete_failed(
        &mut self,
        blob_id: Uuid,
        claim_token: Uuid,
        error_code: &str,
        retry_at: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "UPDATE blobs SET archive_local_delete_error_code = $4, archive_local_delete_error_at = now(), \
                 archive_local_delete_attempts = archive_local_delete_attempts + 1, \
                 archive_local_delete_pending = true, archive_next_attempt_at = $5 \
             WHERE id = $1 AND org_id = $2 AND tier_state = 'archived' \
               AND archive_claim_token = $3",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .bind(error_code)
        .bind(retry_at)
        .execute(&mut *self.tx)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    pub async fn maint_chunk_archive_claim(
        &mut self,
        chunk_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        stale_before: OffsetDateTime,
        claim_token: Uuid,
    ) -> Result<Option<ChunkArchiveClaim>, DbError> {
        let due = CHUNK_ARCHIVE_DUE.replace("$1", "$5");
        let row: Option<ChunkArchiveClaimRow> = sqlx::query_as(&format!(
            "SELECT c.object_key, c.archive_object_key, c.archive_wrapped_key, c.size_bytes, c.sha256, c.stored_bytes, c.encoding \
             FROM chunks c WHERE c.id=$1 AND c.org_id=$2 AND c.state='committed' \
               AND c.storage_tier IN ('local','archiving') \
               AND (c.archive_next_attempt_at IS NULL OR c.archive_next_attempt_at <= $3) \
               AND (c.storage_tier='local' OR c.archive_claimed_at <= $4) \
               AND {due} FOR UPDATE OF c SKIP LOCKED"
        ))
        .bind(chunk_id).bind(self.org_id).bind(now).bind(stale_before).bind(cutoff)
        .fetch_optional(&mut *self.tx).await?;
        let Some((
            object_key,
            archive_object_key,
            archive_wrapped_key,
            size_bytes,
            sha256,
            stored_bytes,
            encoding,
        )) = row
        else {
            return Ok(None);
        };
        sqlx::query("UPDATE chunks SET storage_tier='archiving', archive_claim_token=$3, archive_claimed_at=$4, archive_error_code=NULL WHERE id=$1 AND org_id=$2")
            .bind(chunk_id).bind(self.org_id).bind(claim_token).bind(now)
            .execute(&mut *self.tx).await?;
        Ok(Some(ChunkArchiveClaim {
            chunk_id,
            object_key,
            archive_object_key,
            archive_wrapped_key,
            size_bytes,
            sha256,
            stored_bytes,
            encoding,
            claim_token,
        }))
    }

    pub async fn maint_chunk_archive_intent(
        &mut self,
        chunk_id: Uuid,
        claim_token: Uuid,
        key: &str,
        envelope: &[u8],
    ) -> Result<bool, DbError> {
        let result = sqlx::query("UPDATE chunks SET archive_object_key=$3, archive_wrapped_key=$4 WHERE id=$1 AND org_id=$2 AND storage_tier='archiving' AND archive_claim_token=$5 AND archive_object_key IS NULL AND archive_wrapped_key IS NULL")
            .bind(chunk_id).bind(self.org_id).bind(key).bind(envelope).bind(claim_token)
            .execute(&mut *self.tx).await?;
        if result.rows_affected() == 1 {
            return Ok(true);
        }
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM chunks WHERE id=$1 AND org_id=$2 AND storage_tier='archiving' AND archive_claim_token=$3 AND archive_object_key=$4 AND archive_wrapped_key=$5)")
            .bind(chunk_id).bind(self.org_id).bind(claim_token).bind(key).bind(envelope)
            .fetch_one(&mut *self.tx).await?)
    }

    pub async fn maint_chunk_archive_finish(
        &mut self,
        chunk_id: Uuid,
        token: Uuid,
        key: &str,
        envelope: &[u8],
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<ArchiveFinish, DbError> {
        let row: Option<(String, Option<Uuid>)> = sqlx::query_as("SELECT storage_tier, archive_claim_token FROM chunks WHERE id=$1 AND org_id=$2 AND state='committed' FOR UPDATE")
            .bind(chunk_id).bind(self.org_id).fetch_optional(&mut *self.tx).await?;
        let Some((tier, claimed)) = row else {
            return Ok(ArchiveFinish::StaleClaim);
        };
        if tier != "archiving" || claimed != Some(token) {
            return Ok(ArchiveFinish::StaleClaim);
        }
        let due: bool = sqlx::query_scalar(&format!(
            "SELECT {CHUNK_ARCHIVE_DUE} FROM chunks c WHERE c.id=$2 AND c.org_id=$3"
        ))
        .bind(cutoff)
        .bind(chunk_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?
        .unwrap_or(false);
        if !due {
            sqlx::query("UPDATE chunks SET storage_tier='local', archive_claim_token=NULL, archive_claimed_at=NULL WHERE id=$1 AND org_id=$2")
                .bind(chunk_id).bind(self.org_id).execute(&mut *self.tx).await?;
            return Ok(ArchiveFinish::KeepLocal);
        }
        let done = sqlx::query("UPDATE chunks SET storage_tier='archive', archive_object_key=$3, archive_wrapped_key=$4, archive_verified_at=$5, archive_local_delete_pending=true, archive_error_code=NULL WHERE id=$1 AND org_id=$2 AND storage_tier='archiving' AND archive_claim_token=$6")
            .bind(chunk_id).bind(self.org_id).bind(key).bind(envelope).bind(now).bind(token)
            .execute(&mut *self.tx).await?;
        Ok(if done.rows_affected() == 1 {
            ArchiveFinish::Archived
        } else {
            ArchiveFinish::StaleClaim
        })
    }

    pub async fn maint_chunk_archive_local_delete_target(
        &mut self,
        chunk_id: Uuid,
        token: Uuid,
    ) -> Result<Option<String>, DbError> {
        Ok(sqlx::query_scalar("SELECT object_key FROM chunks c WHERE c.id=$1 AND c.org_id=$2 AND c.storage_tier='archive' AND c.archive_local_delete_pending AND c.archive_claim_token=$3 AND NOT EXISTS(SELECT 1 FROM version_chunks vc JOIN file_versions v ON v.id=vc.version_id JOIN files f ON f.id=v.file_id AND f.org_id=v.org_id WHERE vc.chunk_id=c.id AND f.head_version_id=v.id) AND NOT EXISTS(SELECT 1 FROM upload_chunks u WHERE u.chunk_id=c.id) FOR UPDATE")
            .bind(chunk_id).bind(self.org_id).bind(token).fetch_optional(&mut *self.tx).await?)
    }

    pub async fn maint_chunk_archive_local_delete_finish(
        &mut self,
        chunk_id: Uuid,
        token: Uuid,
    ) -> Result<bool, DbError> {
        let done = sqlx::query("UPDATE chunks SET archive_local_delete_pending=false, archive_claim_token=NULL, archive_claimed_at=NULL, archive_next_attempt_at=NULL WHERE id=$1 AND org_id=$2 AND storage_tier='archive' AND archive_local_delete_pending AND archive_claim_token=$3 AND NOT EXISTS(SELECT 1 FROM version_chunks vc JOIN file_versions v ON v.id=vc.version_id JOIN files f ON f.id=v.file_id AND f.org_id=v.org_id WHERE vc.chunk_id=chunks.id AND f.head_version_id=v.id) AND NOT EXISTS(SELECT 1 FROM upload_chunks u WHERE u.chunk_id=chunks.id)")
            .bind(chunk_id).bind(self.org_id).bind(token).execute(&mut *self.tx).await?;
        Ok(done.rows_affected() == 1)
    }

    pub async fn maint_chunk_archive_local_delete_failed(
        &mut self,
        chunk_id: Uuid,
        token: Uuid,
        code: &str,
        retry_at: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query("UPDATE chunks SET archive_error_code=$3, archive_next_attempt_at=$4 WHERE id=$1 AND org_id=$2 AND archive_local_delete_pending AND archive_claim_token=$5")
            .bind(chunk_id).bind(self.org_id).bind(code).bind(retry_at).bind(token).execute(&mut *self.tx).await?;
        Ok(done.rows_affected() == 1)
    }

    pub async fn maint_chunk_archive_retry(
        &mut self,
        chunk_id: Uuid,
        token: Uuid,
        code: &str,
        retry_at: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query("UPDATE chunks SET storage_tier='local', archive_claim_token=NULL, archive_claimed_at=NULL, archive_error_code=$3, archive_next_attempt_at=$4 WHERE id=$1 AND org_id=$2 AND storage_tier='archiving' AND archive_claim_token=$5")
            .bind(chunk_id).bind(self.org_id).bind(code).bind(retry_at).bind(token).execute(&mut *self.tx).await?;
        Ok(done.rows_affected() == 1)
    }

    /// Claims an archived blob for return to local storage. Archive bytes stay
    /// authoritative until the verified local copy is finalized.
    pub async fn maint_archive_recall_claim(
        &mut self,
        blob_id: Uuid,
        now: OffsetDateTime,
        stale_before: OffsetDateTime,
        claim_token: Uuid,
    ) -> Result<Option<ArchiveRecallClaim>, DbError> {
        let row: Option<ArchiveRecallRow> = sqlx::query_as(
            "SELECT object_key, archive_object_key, archive_wrapped_key, size_bytes, sha256, \
                        archive_claimed_at \
                 FROM blobs WHERE id = $1 AND org_id = $2 AND state = 'committed' \
                   AND tier_state IN ('archived', 'recalling') AND archive_object_key IS NOT NULL \
                   AND archive_wrapped_key IS NOT NULL \
                   AND (archive_claim_token IS NULL OR archive_claimed_at <= $3) \
                 FOR UPDATE SKIP LOCKED",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(stale_before)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((
            object_key,
            Some(archive_object_key),
            Some(archive_wrapped_key),
            size_bytes,
            sha256,
            _,
        )) = row
        else {
            return Ok(None);
        };
        sqlx::query(
            "UPDATE blobs SET tier_state = 'recalling', archive_claim_token = $3, \
                 archive_claimed_at = $4, archive_error_code = NULL, archive_error_at = NULL, \
                 archive_local_delete_pending = false \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .bind(now)
        .execute(&mut *self.tx)
        .await?;
        Ok(Some(ArchiveRecallClaim {
            blob_id,
            object_key,
            archive_object_key,
            archive_wrapped_key,
            size_bytes,
            sha256,
            claim_token,
        }))
    }

    /// Marks a verified local copy ready. Archive reference remains intact.
    pub async fn maint_archive_recall_finish(
        &mut self,
        blob_id: Uuid,
        claim_token: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "UPDATE blobs SET tier_state = 'local', archive_claim_token = NULL, \
                 archive_claimed_at = NULL, archive_verify_claim_token = NULL, \
                 archive_verify_claimed_at = NULL, archive_verify_offset = 0, \
                 archive_verify_attempted_at = NULL, archive_verify_no_progress_attempts = 0, \
                 archive_error_code = NULL, archive_error_at = NULL, \
                 archive_next_attempt_at = NULL, archive_verified_at = $4, \
                 archive_local_delete_pending = false \
             WHERE id = $1 AND org_id = $2 AND tier_state = 'recalling' \
               AND archive_claim_token = $3",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .bind(now)
        .execute(&mut *self.tx)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Records an archive attempt failure and schedules retry without losing a
    /// published object reference.
    pub async fn maint_archive_retry(
        &mut self,
        blob_id: Uuid,
        claim_token: Uuid,
        error_code: &str,
        retry_at: OffsetDateTime,
        archive_object_key: Option<&str>,
        archive_wrapped_key: Option<&[u8]>,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "UPDATE blobs SET tier_state = 'local', archive_claim_token = NULL, \
                 archive_claimed_at = NULL, archive_error_code = $4, archive_error_at = now(), \
                 archive_next_attempt_at = $5, \
                 archive_object_key = COALESCE($6, archive_object_key), \
                 archive_wrapped_key = COALESCE($7, archive_wrapped_key) \
             WHERE id = $1 AND org_id = $2 AND tier_state = 'archiving' \
               AND archive_claim_token = $3",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .bind(error_code)
        .bind(retry_at)
        .bind(archive_object_key)
        .bind(archive_wrapped_key)
        .execute(&mut *self.tx)
        .await?;
        if done.rows_affected() == 1 {
            return Ok(true);
        }
        let recalled = sqlx::query(
            "UPDATE blobs SET tier_state = 'archived', archive_claim_token = NULL, \
                 archive_claimed_at = NULL, archive_error_code = $4, archive_error_at = now(), \
                 archive_next_attempt_at = $5, archive_local_delete_pending = false \
             WHERE id = $1 AND org_id = $2 AND tier_state = 'recalling' \
               AND archive_claim_token = $3",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .bind(error_code)
        .bind(retry_at)
        .execute(&mut *self.tx)
        .await?;
        Ok(recalled.rows_affected() == 1)
    }
}

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
           AND b.tier_state NOT IN ('archiving', 'recalling') AND b.archive_claim_token IS NULL \
           AND b.archive_verify_claim_token IS NULL \
           AND (b.archive_object_key IS NULL OR ( \
                COALESCE((SELECT inventory_complete FROM archive_gc_state WHERE id), false) \
                AND NOT EXISTS (SELECT 1 FROM archive_backup_refs r \
                                WHERE r.blob_id = b.id OR r.archive_object_key = b.archive_object_key))) \
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

/// A deleting blob, including its optional archive object.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DeletingBlob {
    /// Organization id.
    pub org_id: Uuid,
    /// Blob id.
    pub id: Uuid,
    /// Local object key.
    pub object_key: String,
    /// Archive object key, if one must also be deleted.
    pub archive_object_key: Option<String>,
    /// Whether the archive delete still needs completion.
    pub archive_delete_pending: bool,
}

/// Deleting blobs with all cleanup targets. The legacy `gc_deleting` remains
/// available for callers that only clean local objects.
pub async fn gc_deleting_targets(pool: &PgPool, limit: i64) -> Result<Vec<DeletingBlob>, DbError> {
    Ok(sqlx::query_as(
        "SELECT org_id, id, object_key, archive_object_key, archive_delete_pending \
         FROM blobs WHERE state = 'deleting' \
           AND (archive_delete_next_attempt_at IS NULL OR archive_delete_next_attempt_at <= now()) \
           AND (NOT archive_delete_pending OR ( \
                COALESCE((SELECT inventory_complete FROM archive_gc_state WHERE id), false) \
                AND NOT EXISTS (SELECT 1 FROM archive_backup_refs r \
                                WHERE r.blob_id = blobs.id OR r.archive_object_key = blobs.archive_object_key))) \
         ORDER BY created_at, id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// A chunk eligible for physical deletion, or already claimed by an earlier run.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DeletingChunk {
    /// Organization id.
    pub org_id: Uuid,
    /// Chunk id.
    pub id: Uuid,
    /// Local object key.
    pub object_key: String,
    /// Archive copy to delete after local object removal.
    pub archive_object_key: Option<String>,
}

/// Lists chunk objects whose durable deletion claim must be resumed.
pub async fn gc_deleting_chunks(pool: &PgPool, limit: i64) -> Result<Vec<DeletingChunk>, DbError> {
    Ok(sqlx::query_as(
        "SELECT org_id, id, object_key, archive_object_key FROM chunks WHERE state = 'deleting' \
         ORDER BY created_at, id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Lists unreferenced chunks old enough for GC.
pub async fn gc_chunk_candidates(
    pool: &PgPool,
    created_before: OffsetDateTime,
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT c.org_id, c.id FROM chunks c WHERE c.state = 'orphaned' \
         AND c.created_at < $1 \
         AND NOT EXISTS (SELECT 1 FROM version_chunks v WHERE v.chunk_id = c.id) \
         AND NOT EXISTS (SELECT 1 FROM upload_chunks u WHERE u.chunk_id = c.id) \
         AND NOT EXISTS (SELECT 1 FROM archive_chunk_backup_refs r WHERE r.chunk_id = c.id) \
         ORDER BY c.created_at, c.id LIMIT $2",
    )
    .bind(created_before)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// A claimed local chunk object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkGcClaim {
    /// Object key.
    pub object_key: String,
    /// Archive copy, if this chunk was archived.
    pub archive_object_key: Option<String>,
}

impl OrgTx {
    /// Claims a chunk only while it has no upload or version references.
    pub async fn maint_chunk_gc_claim(
        &mut self,
        chunk_id: Uuid,
        created_before: OffsetDateTime,
    ) -> Result<Option<ChunkGcClaim>, DbError> {
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT object_key, archive_object_key FROM chunks c WHERE c.id = $1 AND c.org_id = $2 \
             AND c.state = 'orphaned' AND c.created_at < $3 \
             AND NOT EXISTS (SELECT 1 FROM version_chunks v WHERE v.chunk_id = c.id) \
             AND NOT EXISTS (SELECT 1 FROM upload_chunks u WHERE u.chunk_id = c.id) \
             AND NOT EXISTS (SELECT 1 FROM archive_chunk_backup_refs r WHERE r.chunk_id = c.id) \
             AND (c.archive_object_key IS NULL OR (COALESCE((SELECT inventory_complete FROM archive_gc_state WHERE id), false) \
                  AND NOT EXISTS (SELECT 1 FROM archive_chunk_backup_refs r WHERE r.chunk_id=c.id OR r.archive_object_key=c.archive_object_key))) \
             FOR UPDATE SKIP LOCKED",
        )
        .bind(chunk_id)
        .bind(self.org_id)
        .bind(created_before)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((object_key, archive_object_key)) = row else {
            return Ok(None);
        };
        sqlx::query("UPDATE chunks SET state = 'deleting' WHERE id = $1 AND org_id = $2")
            .bind(chunk_id)
            .bind(self.org_id)
            .execute(&mut *self.tx)
            .await?;
        Ok(Some(ChunkGcClaim {
            object_key,
            archive_object_key,
        }))
    }

    /// Removes the metadata after local object deletion succeeded.
    pub async fn maint_chunk_gc_finish(&mut self, chunk_id: Uuid) -> Result<bool, DbError> {
        let deleted = sqlx::query(
            "DELETE FROM chunks c WHERE c.id = $1 AND c.org_id = $2 AND c.state = 'deleting' \
             AND NOT EXISTS (SELECT 1 FROM version_chunks v WHERE v.chunk_id = c.id) \
             AND NOT EXISTS (SELECT 1 FROM upload_chunks u WHERE u.chunk_id = c.id) \
             AND NOT EXISTS (SELECT 1 FROM archive_chunk_backup_refs r WHERE r.chunk_id = c.id) \
             AND (c.archive_object_key IS NULL OR (COALESCE((SELECT inventory_complete FROM archive_gc_state WHERE id), false) \
                  AND NOT EXISTS (SELECT 1 FROM archive_chunk_backup_refs r WHERE r.chunk_id=c.id OR r.archive_object_key=c.archive_object_key)))",
        )
        .bind(chunk_id)
        .bind(self.org_id)
        .execute(&mut *self.tx)
        .await?
        .rows_affected()
            == 1;
        Ok(deleted)
    }
}

/// Phase 2 of the garbage collection, after the object is gone: deletes the
/// row of a `deleting` blob. Returns whether a row was removed.
///
/// # Errors
///
/// Database failures (a foreign key violation would mean something still
/// references the blob; the row then stays `deleting`).
pub async fn gc_finish(pool: &PgPool, blob_id: Uuid) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let quota_group_id: Option<Uuid> = sqlx::query_scalar(
        "DELETE FROM blobs WHERE id = $1 AND state = 'deleting' \
           AND NOT archive_delete_pending \
           AND NOT EXISTS (SELECT 1 FROM archive_backup_refs r \
                           WHERE r.blob_id = blobs.id OR r.archive_object_key = blobs.archive_object_key) \
         RETURNING quota_group_id",
    )
    .bind(blob_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(quota_group_id) = quota_group_id {
        sqlx::query(
            "DELETE FROM blob_quota_groups q WHERE q.id = $1 \
             AND NOT EXISTS (SELECT 1 FROM blobs b WHERE b.quota_group_id = q.id)",
        )
        .bind(quota_group_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(quota_group_id.is_some())
}

/// Records an archive object delete failure for durable retry and operator status.
pub async fn gc_archive_delete_failed(
    pool: &PgPool,
    blob_id: Uuid,
    error_code: &str,
    retry_at: OffsetDateTime,
) -> Result<bool, DbError> {
    let done = sqlx::query(
        "UPDATE blobs SET archive_delete_error_code = $2, archive_delete_error_at = now(), \
             archive_delete_attempts = archive_delete_attempts + 1, \
             archive_delete_next_attempt_at = $3 \
         WHERE id = $1 AND state = 'deleting' AND archive_delete_pending \
           AND COALESCE((SELECT inventory_complete FROM archive_gc_state WHERE id), false) \
           AND NOT EXISTS (SELECT 1 FROM archive_backup_refs r \
                           WHERE r.blob_id = blobs.id OR r.archive_object_key = blobs.archive_object_key)",
    )
    .bind(blob_id)
    .bind(error_code)
    .bind(retry_at)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Marks the archive copy removed. `gc_finish` may then remove the blob row.
pub async fn gc_archive_delete_finish(pool: &PgPool, blob_id: Uuid) -> Result<bool, DbError> {
    let done = sqlx::query(
        "UPDATE blobs SET archive_delete_pending = false, archive_delete_error_code = NULL, \
             archive_delete_error_at = NULL, \
             archive_delete_next_attempt_at = NULL \
         WHERE id = $1 AND state = 'deleting' AND archive_delete_pending \
           AND COALESCE((SELECT inventory_complete FROM archive_gc_state WHERE id), false) \
           AND NOT EXISTS (SELECT 1 FROM archive_backup_refs r \
                           WHERE r.blob_id = blobs.id OR r.archive_object_key = blobs.archive_object_key)",
    )
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
    /// Archive object key to delete after the local object.
    pub archive_object_key: Option<String>,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Whether the blob was the last committed copy of its quota group or an
    /// `orphaned` upload, and its group counter was subtracted by the claim.
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

    /// Subtracts reserved bytes, never below zero. Returns whether a counter
    /// had to be clamped.
    async fn sub_reserved(&mut self, workspace_id: Uuid, bytes: i64) -> Result<bool, DbError> {
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
        let clamped = ws.is_none_or(|current| current < bytes) || org < bytes;
        sqlx::query(
            "UPDATE workspaces SET reserved_bytes = GREATEST(reserved_bytes - $3, 0) \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(workspace_id)
        .bind(self.org_id)
        .bind(bytes)
        .execute(&mut *self.tx)
        .await?;
        sqlx::query(
            "UPDATE organizations SET reserved_bytes = GREATEST(reserved_bytes - $2, 0) WHERE id = $1",
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
    /// committed counters if it was counted. An `orphaned` blob (an aborted or
    /// expired stored upload) is subtracted from the reserved counters: its
    /// bytes counted against the quota until now. The caller commits and then
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
        let row: Option<(String, Option<String>, i64, Uuid, String, Uuid)> = sqlx::query_as(
            "SELECT object_key, archive_object_key, size_bytes, workspace_id, state, quota_group_id FROM blobs \
             WHERE id = $1 AND org_id = $2 AND state IN ('orphaned', 'committed', 'corrupt') \
               AND created_at < $3 AND tier_state NOT IN ('archiving', 'recalling') \
               AND archive_claim_token IS NULL AND archive_verify_claim_token IS NULL FOR UPDATE SKIP LOCKED",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(created_before)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((object_key, archive_object_key, size_bytes, workspace_id, state, quota_group_id)) =
            row
        else {
            return Ok(GcClaim::Skipped);
        };
        if archive_object_key.is_some() {
            let blocked: bool = sqlx::query_scalar(
                "SELECT NOT COALESCE((SELECT inventory_complete FROM archive_gc_state WHERE id), false) \
                 OR EXISTS (SELECT 1 FROM archive_backup_refs r \
                            WHERE r.blob_id = $1 OR r.archive_object_key = $2)",
            )
            .bind(blob_id)
            .bind(archive_object_key.as_deref())
            .fetch_one(&mut *self.tx)
            .await?;
            if blocked {
                return Ok(GcClaim::Skipped);
            }
        }
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
        sqlx::query(
            "UPDATE blobs SET state = 'deleting', archive_delete_pending = archive_object_key IS NOT NULL, \
                 archive_delete_error_code = NULL, archive_delete_error_at = NULL, archive_delete_next_attempt_at = NULL, \
                 archive_claim_token = NULL, archive_claimed_at = NULL \
             WHERE id = $1 AND org_id = $2",
        )
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
        let other_counted_copy: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM blobs WHERE org_id = $1 AND quota_group_id = $2 \
             AND id <> $3 AND state IN ('committed', 'corrupt'))",
        )
        .bind(self.org_id)
        .bind(quota_group_id)
        .bind(blob_id)
        .fetch_one(&mut *self.tx)
        .await?;
        let last_committed_copy =
            matches!(state.as_str(), "committed" | "corrupt") && !other_counted_copy;
        let counted = last_committed_copy || state == "orphaned";
        let clamped = match state.as_str() {
            "committed" | "corrupt" if last_committed_copy => {
                self.sub_committed(workspace_id, size_bytes).await?
            }
            "committed" | "corrupt" => false,
            "orphaned" => self.sub_reserved(workspace_id, size_bytes).await?,
            _ => false,
        };
        Ok(GcClaim::Claimed(ClaimedBlob {
            object_key,
            archive_object_key,
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
    let found: Vec<String> = sqlx::query_scalar(
        "SELECT object_key FROM blobs WHERE layout = 'whole' AND object_key = ANY($1) \
         UNION SELECT object_key FROM chunks WHERE object_key = ANY($1)",
    )
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
    /// `whole` or `chunked`.
    pub layout: String,
    /// A representative file mapping for chunked verification.
    pub file_id: Option<Uuid>,
    /// A representative version mapping for chunked verification.
    pub version_id: Option<Uuid>,
}

/// The committed and corrupt blobs that were verified longest ago (never
/// verified first).
///
/// # Errors
///
/// Database failures only.
pub async fn verify_candidates(pool: &PgPool, limit: i64) -> Result<Vec<VerifyCandidate>, DbError> {
    Ok(sqlx::query_as(
        "SELECT b.id, b.object_key, b.size_bytes, b.sha256, b.state, b.layout, \
                mapping.file_id, mapping.version_id \
         FROM blobs b LEFT JOIN LATERAL ( \
             SELECT file_id, id AS version_id FROM file_versions WHERE blob_id=b.id \
             ORDER BY created_at, id LIMIT 1 \
         ) mapping ON true \
         WHERE b.state IN ('committed', 'corrupt') AND b.tier_state IN ('local', 'archiving') \
           AND (b.layout = 'whole' OR (b.layout = 'chunked' AND mapping.version_id IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM version_chunks vc JOIN chunks c ON c.id=vc.chunk_id \
                             WHERE vc.version_id=mapping.version_id AND c.storage_tier='archive'))) \
         ORDER BY b.verified_at NULLS FIRST, b.created_at, b.id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Loads the bounded per-file manifest for one maintenance verification candidate.
/// Candidate identifiers originate from `verify_candidates`; this path is only
/// used by the global maintenance runner and deliberately has no user scope.
pub async fn verify_candidate_chunks(
    pool: &PgPool,
    blob_id: Uuid,
    file_id: Uuid,
    version_id: Uuid,
) -> Result<Vec<crate::versions::DownloadChunk>, DbError> {
    Ok(sqlx::query_as(
        "SELECT c.id AS chunk_id, vc.ordinal, vc.byte_offset, c.object_key, c.size_bytes, c.stored_bytes, c.encoding, \
                c.digest, c.sha256, vc.file_id, c.storage_tier, c.archive_object_key, \
                c.archive_wrapped_key \
         FROM version_chunks vc JOIN chunks c ON c.id=vc.chunk_id \
         JOIN file_versions v ON v.id=vc.version_id AND v.file_id=vc.file_id \
         WHERE v.id=$1 AND v.file_id=$2 AND v.blob_id=$3 ORDER BY vc.ordinal",
    )
    .bind(version_id)
    .bind(file_id)
    .bind(blob_id)
    .fetch_all(pool)
    .await?)
}

/// An archived object due for remote integrity verification.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ArchiveVerifyCandidate {
    /// Blob id, also the encryption AAD identity.
    pub id: Uuid,
    /// Archive object key.
    pub archive_object_key: String,
    /// Wrapped per-object data key.
    pub archive_wrapped_key: Vec<u8>,
    /// Recorded plaintext size.
    pub size_bytes: i64,
    /// Recorded plaintext SHA-256.
    pub sha256: Vec<u8>,
    /// Plaintext offset through authenticated blocks already checked.
    pub verified_offset: i64,
    /// Fencing token for this verification run.
    pub claim_token: Uuid,
}

/// Claims one archived blob for a bounded verification run. Stale claims can
/// be resumed from their durable block offset; each run receives a new token.
pub async fn archive_verify_claim(
    pool: &PgPool,
    now: OffsetDateTime,
    stale_before: OffsetDateTime,
    due_before: OffsetDateTime,
) -> Result<Option<ArchiveVerifyCandidate>, DbError> {
    let token = Uuid::new_v4();
    let row = sqlx::query_as(
        "WITH candidate AS (SELECT id FROM blobs WHERE state = 'committed' \
           AND tier_state = 'archived' AND archive_object_key IS NOT NULL \
           AND archive_wrapped_key IS NOT NULL \
           AND (archive_verified_at IS NULL OR archive_verified_at <= $3) \
           AND (archive_verify_claimed_at IS NULL OR archive_verify_claimed_at <= $2) \
           ORDER BY archive_verify_attempted_at NULLS FIRST, archive_verified_at NULLS FIRST, created_at, id \
           LIMIT 1 FOR UPDATE SKIP LOCKED) \
         UPDATE blobs b SET archive_verify_claim_token = $4, archive_verify_claimed_at = $1, \
           archive_verify_attempted_at = $1 \
         FROM candidate c WHERE b.id = c.id \
         RETURNING b.id, b.archive_object_key, b.archive_wrapped_key, b.size_bytes, \
           b.sha256, b.archive_verify_offset AS verified_offset, \
           b.archive_verify_claim_token AS claim_token",
    )
    .bind(now)
    .bind(stale_before)
    .bind(due_before)
    .bind(token)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Saves progress only for the current verification lease, then releases it.
/// A completed pass records successful verification and resets its offset.
pub async fn archive_verify_progress(
    pool: &PgPool,
    candidate: &ArchiveVerifyCandidate,
    offset: i64,
    complete: bool,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    type VerifyState = (Uuid, i64, Option<String>, Option<OffsetDateTime>, i32);
    let current: Option<VerifyState> = sqlx::query_as(
        "SELECT org_id, archive_verify_offset, archive_error_code, archive_error_at, \
               archive_verify_no_progress_attempts FROM blobs \
             WHERE id = $1 AND state = 'committed' AND tier_state = 'archived' \
               AND archive_verify_claim_token = $2 AND archive_verify_claimed_at > $3 FOR UPDATE",
    )
    .bind(candidate.id)
    .bind(candidate.claim_token)
    .bind(now - time::Duration::minutes(15))
    .fetch_optional(&mut *tx)
    .await?;
    let Some((org_id, previous_offset, previous_error, previous_error_at, previous_attempts)) =
        current
    else {
        tx.commit().await?;
        return Ok(false);
    };
    let no_progress_attempts = if complete || offset != previous_offset {
        0
    } else {
        previous_attempts.saturating_add(1)
    };
    let report_no_progress = !complete && previous_error.is_none() && no_progress_attempts >= 3;
    let error_code = if complete {
        None
    } else if report_no_progress {
        Some("ARCHIVE_VERIFY_NO_PROGRESS".to_owned())
    } else {
        previous_error.clone()
    };
    let error_at = if complete {
        None
    } else if report_no_progress {
        Some(now)
    } else {
        previous_error_at
    };
    sqlx::query(
        "UPDATE blobs SET archive_verified_at = CASE WHEN $4 THEN GREATEST( \
             $5, COALESCE(archive_verified_at, $5) + interval '1 microsecond') \
             ELSE archive_verified_at END, archive_error_code = $6, archive_error_at = $7, \
           archive_verify_no_progress_attempts = $8, \
           archive_verify_claim_token = NULL, archive_verify_claimed_at = NULL, \
           archive_verify_offset = CASE WHEN $4 THEN 0 ELSE $3 END \
         WHERE id = $1 AND archive_verify_claim_token = $2",
    )
    .bind(candidate.id)
    .bind(candidate.claim_token)
    .bind(offset)
    .bind(complete)
    .bind(now)
    .bind(&error_code)
    .bind(error_at)
    .bind(no_progress_attempts)
    .execute(&mut *tx)
    .await?;
    if report_no_progress {
        let event = NewEvent::new("archive.verification_failed")
            .org(org_id)
            .target("blob", candidate.id)
            .result(crate::audit::AuditResult::Failed)
            .detail("error_code", Detail::Code("ARCHIVE_VERIFY_NO_PROGRESS"));
        crate::audit::record_in(&mut tx, &AuditCtx::system(), &event).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Releases a failed verification lease, resets partial progress, and records
/// a bounded operator-visible error code.
pub async fn archive_verify_failed(
    pool: &PgPool,
    candidate: &ArchiveVerifyCandidate,
    error_code: &'static str,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let current: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT org_id, archive_error_code FROM blobs WHERE id = $1 \
         AND state = 'committed' AND tier_state = 'archived' \
         AND archive_verify_claim_token = $2 FOR UPDATE",
    )
    .bind(candidate.id)
    .bind(candidate.claim_token)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((org_id, previous_error)) = current else {
        tx.commit().await?;
        return Ok(false);
    };
    sqlx::query(
        "UPDATE blobs SET archive_verify_offset = 0, archive_verify_claim_token = NULL, \
           archive_verify_claimed_at = NULL, archive_verify_attempted_at = $4, \
           archive_verify_no_progress_attempts = 0, archive_verified_at = GREATEST( \
             $4, COALESCE(archive_verified_at, $4) + interval '1 microsecond'), \
           archive_error_code = $3, archive_error_at = $4 \
         WHERE id = $1 AND archive_verify_claim_token = $2",
    )
    .bind(candidate.id)
    .bind(candidate.claim_token)
    .bind(error_code)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if previous_error.as_deref() != Some(error_code) {
        let event = NewEvent::new("archive.verification_failed")
            .org(org_id)
            .target("blob", candidate.id)
            .result(crate::audit::AuditResult::Failed)
            .detail("error_code", Detail::Code(error_code));
        crate::audit::record_in(&mut tx, &AuditCtx::system(), &event).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Records an archive verification. Error codes are bounded machine values and
/// never carry provider messages, paths or credentials.
pub async fn record_archive_verification(
    pool: &PgPool,
    blob_id: Uuid,
    error_code: Option<&'static str>,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let current: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT org_id, archive_error_code FROM blobs \
         WHERE id = $1 AND state = 'committed' AND tier_state = 'archived' FOR UPDATE",
    )
    .bind(blob_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((org_id, previous_error)) = current else {
        tx.commit().await?;
        return Ok(false);
    };
    sqlx::query(
        "UPDATE blobs SET archive_verified_at = GREATEST( \
             $3, COALESCE(archive_verified_at, $3) + interval '1 microsecond'), \
             archive_error_code = $2, \
             archive_error_at = CASE WHEN $2 IS NULL THEN NULL ELSE $3 END \
         WHERE id = $1 AND state = 'committed' AND tier_state = 'archived'",
    )
    .bind(blob_id)
    .bind(error_code)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if let Some(code) = error_code.filter(|code| previous_error.as_deref() != Some(*code)) {
        let event = NewEvent::new("archive.verification_failed")
            .org(org_id)
            .target("blob", blob_id)
            .result(crate::audit::AuditResult::Failed)
            .detail("error_code", Detail::Code(code));
        crate::audit::record_in(&mut tx, &AuditCtx::system(), &event).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Records an archive head/length check without clearing prior integrity errors.
/// This timestamp does not assert that ciphertext was decrypted or hashed.
pub async fn record_archive_head_check(
    pool: &PgPool,
    blob_id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let done = sqlx::query(
        "UPDATE blobs SET archive_verified_at = GREATEST( \
             $2, COALESCE(archive_verified_at, $2) + interval '1 microsecond') \
         WHERE id = $1 AND state = 'committed' AND tier_state = 'archived'",
    )
    .bind(blob_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
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
                          WHERE s.workspace_id = w.id AND s.state IN {HOLDING}), 0) \
                + COALESCE((SELECT sum(b.size_bytes)::bigint FROM blobs b \
                          WHERE b.workspace_id = w.id AND b.state = 'orphaned'), 0) AS er, \
                COALESCE((SELECT sum(q.size_bytes)::bigint FROM blob_quota_groups q \
                          WHERE q.org_id = w.org_id AND q.workspace_id = w.id \
                            AND EXISTS (SELECT 1 FROM blobs b WHERE b.quota_group_id = q.id \
                              AND b.org_id = q.org_id AND b.workspace_id = q.workspace_id \
                              AND b.state IN ('committed', 'corrupt'))), 0) AS ec \
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

#[cfg(test)]
mod thinning_tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_794_012_000).unwrap()
    }

    fn version(number: i64, created_at: OffsetDateTime) -> RetentionVersion {
        RetentionVersion {
            id: Uuid::from_u128(number as u128),
            number,
            created_at,
            pinned: false,
            protected: false,
        }
    }

    fn defaults() -> VersionThinningPolicy {
        VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 24,
            daily_days: 7,
            weekly_weeks: 4,
            max_per_file: 0,
        }
    }

    #[test]
    fn empty_and_head_only_files_are_safe() {
        assert!(versions_to_keep(&[], None, defaults(), now()).is_empty());
        let head = version(1, now());
        assert_eq!(
            versions_to_keep(&[head], Some(head.id), defaults(), now()),
            HashSet::from([head.id])
        );
    }

    #[test]
    fn hourly_versions_over_sixty_days_fit_default_grid() {
        let now = now();
        let versions = (0..1_440)
            .map(|age| {
                version(
                    i64::from(age + 1),
                    now - time::Duration::hours(i64::from(age)),
                )
            })
            .collect::<Vec<_>>();
        let head = versions.first().unwrap().id;
        let kept = versions_to_keep(&versions, Some(head), defaults(), now);
        assert!(kept.len() <= 36, "kept {} versions", kept.len());
        assert!(kept.contains(&head));
        let hundred = (0..100)
            .map(|index| {
                version(
                    i64::from(index + 1),
                    now - time::Duration::hours(i64::from(index) * 14),
                )
            })
            .collect::<Vec<_>>();
        let hundred_head = hundred.first().unwrap().id;
        let hundred_kept = versions_to_keep(&hundred, Some(hundred_head), defaults(), now);
        assert!(hundred_kept.contains(&hundred_head));
        assert!(
            hundred_kept
                .iter()
                .all(|id| hundred.iter().any(|version| version.id == *id))
        );
    }

    #[test]
    fn daily_sampling_uses_utc_calendar_boundaries_and_latest_tie_break() {
        let now = now();
        let midnight = now.date().with_hms(0, 0, 0).unwrap().assume_utc();
        let versions = vec![
            version(1, midnight - time::Duration::minutes(1)),
            version(2, midnight + time::Duration::minutes(30)),
            version(3, midnight + time::Duration::minutes(30)),
        ];
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 0,
            daily_days: 1,
            weekly_weeks: 0,
            max_per_file: 0,
        };
        assert_eq!(
            versions_to_keep(&versions, None, policy, now),
            HashSet::from([versions[2].id])
        );
    }

    #[test]
    fn weekly_sampling_handles_iso_year_change() {
        let now = Date::from_calendar_date(2024, time::Month::January, 1)
            .unwrap()
            .midnight()
            .assume_utc();
        let versions = vec![
            version(
                1,
                Date::from_calendar_date(2023, time::Month::December, 28)
                    .unwrap()
                    .midnight()
                    .assume_utc(),
            ),
            version(2, now),
        ];
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 0,
            daily_days: 0,
            weekly_weeks: 2,
            max_per_file: 0,
        };
        assert_eq!(
            versions_to_keep(&versions, None, policy, now),
            HashSet::from([versions[0].id, versions[1].id])
        );
    }

    #[test]
    fn weekly_sampling_handles_leap_day() {
        let leap = Date::from_calendar_date(2024, time::Month::February, 29)
            .unwrap()
            .midnight()
            .assume_utc();
        let versions = vec![version(1, leap - time::Duration::days(7)), version(2, leap)];
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 0,
            daily_days: 0,
            weekly_weeks: 2,
            max_per_file: 0,
        };
        let kept = versions_to_keep(&versions, None, policy, leap + time::Duration::hours(1));
        assert_eq!(kept, HashSet::from([versions[0].id, versions[1].id]));
    }

    #[test]
    fn zero_grid_disables_sampling_and_head_remains() {
        let now = now();
        let versions = vec![version(1, now - time::Duration::days(2)), version(2, now)];
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 0,
            daily_days: 0,
            weekly_weeks: 0,
            max_per_file: 0,
        };
        assert_eq!(
            versions_to_keep(&versions, Some(versions[1].id), policy, now),
            HashSet::from([versions[0].id, versions[1].id])
        );
    }

    #[test]
    fn zero_grid_successor_uses_version_number_order() {
        let now = now();
        let versions = vec![
            version(1, now - time::Duration::days(40)),
            version(2, now - time::Duration::days(10)),
            version(3, now - time::Duration::days(50)),
        ];
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 0,
            daily_days: 0,
            weekly_weeks: 0,
            max_per_file: 0,
        };
        assert_eq!(
            versions_to_keep(&versions, None, policy, now),
            HashSet::from([versions[0].id]),
        );
    }

    #[test]
    fn hundred_thousand_versions_apply_cap_in_one_history_pass() {
        let now = now();
        let versions = (0..100_000)
            .map(|index| {
                version(
                    i64::from(index + 1),
                    now - time::Duration::seconds(i64::from(100_000 - index)),
                )
            })
            .collect::<Vec<_>>();
        let head = versions.last().unwrap().id;
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 24,
            daily_days: 7,
            weekly_weeks: 4,
            max_per_file: 2,
        };
        let kept = versions_to_keep(&versions, Some(head), policy, now);
        assert_eq!(kept.len(), 2);
        assert!(kept.contains(&head));
    }

    #[test]
    fn cap_keeps_head_and_oldest_pins_do_not_count() {
        let now = now();
        let mut versions = (0..8)
            .map(|n| version(n + 1, now - time::Duration::days(8 - n)))
            .collect::<Vec<_>>();
        versions[0].pinned = true;
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 0,
            daily_days: 0,
            weekly_weeks: 0,
            max_per_file: 2,
        };
        let kept = versions_to_keep(&versions, Some(versions[7].id), policy, now);
        assert_eq!(
            kept,
            HashSet::from([versions[0].id, versions[6].id, versions[7].id])
        );
    }

    #[test]
    fn published_and_upload_protected_versions_survive_the_cap() {
        let now = now();
        let mut versions = (0..5)
            .map(|n| version(n + 1, now - time::Duration::days(5 - n)))
            .collect::<Vec<_>>();
        versions[1].protected = true;
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 0,
            daily_days: 0,
            weekly_weeks: 0,
            max_per_file: 2,
        };
        let kept = versions_to_keep(&versions, Some(versions[4].id), policy, now);
        assert!(kept.contains(&versions[1].id));
        assert!(kept.contains(&versions[4].id));
    }

    #[test]
    fn version_created_after_the_maintenance_clock_is_preserved() {
        let now = now();
        let versions = vec![
            version(1, now - time::Duration::days(40)),
            version(2, now + time::Duration::seconds(1)),
        ];
        let policy = VersionThinningPolicy {
            retention_days: 30,
            keep_all_hours: 0,
            daily_days: 1,
            weekly_weeks: 0,
            max_per_file: 2,
        };
        let kept = versions_to_keep(&versions, None, policy, now);
        assert_eq!(kept, HashSet::from([versions[1].id]));
    }
}
