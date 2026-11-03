//! The maintenance job (CC-F10, CC-P02-06, CC-P02-07) and its operator
//! status route.
//!
//! One run does, in this order, with bounded batches per step:
//!
//! 1. `expire_uploads`: expires due upload sessions of all organizations
//!    (reservations released, staged blobs orphaned);
//! 2. `purge_trash`: permanently deletes files and projects that have been in
//!    the trash longer than the retention;
//! 3. `trim_versions`: deletes old non-head versions (version retention);
//! 4. `collect_blobs`: two-phase blob garbage collection;
//! 5. `reconcile_storage`: removes stray objects and abandoned temporary files
//!    older than the grace period;
//! 6. `verify_blobs`: re-hashes a bounded number of blobs, oldest verification
//!    first;
//! 7. `fix_usage`: corrects quota counters that differ from the rows;
//! 8. `purge_audit`: deletes audit events older than the audit retention
//!    (CC-F11; database clock, no store involved, see the step).
//!
//! **Safety rails.** The object store is bound to the database: a store id in
//! `storage_binding` and a marker file `craftcloud-store.json` in the store
//! directory must agree, otherwise the storage steps (garbage collection,
//! reconciliation, verification) do nothing and report `STORE_BINDING_MISMATCH`
//! (a wrong or fresh database must never make reconciliation wipe a store).
//! Reconciliation never deletes a stray object directly: it moves it to
//! `quarantine/<yyyymmdd>/` and purges old quarantine days later; a brake aborts
//! the step (`RECONCILE_BRAKE`) when too many objects look unknown. If the
//! application clock and the database clock differ by more than five minutes the
//! destructive steps are skipped (`CLOCK_DRIFT`). A failing item never blocks
//! the other items of its step.
//!
//! A step that fails is logged and reported with an error code; the other
//! steps still run. Only one run exists at a time across all processes (a
//! PostgreSQL advisory lock; a run that cannot get it is skipped). A run is
//! bounded in time and stops between items when the server shuts down. All
//! decisions use the injected [`Clock`]; only the run's time budget uses the
//! monotonic clock. Logs and reports carry counts, ids and error codes, never
//! names, paths or hashes.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::Action;
use craftcloud_db::maintenance::{self as db, GcClaim, VerifyResult};
use craftcloud_db::members::OrgTx;
use craftcloud_db::{DbError, PgPool};
use craftcloud_storage::{
    ArchiveStore, LocalStore, MarkerInfo, ObjectInfo, ObjectKey, ObjectStore, StorageError,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::watch;
use uuid::Uuid;

use crate::orgs::{db_failure, operator_gate, rfc3339};
use crate::responses::unavailable;
use crate::session::AuthenticatedSession;
use crate::{ApiState, Clock};

enum ChunkVerificationError {
    Missing,
    Integrity,
    Storage,
}

async fn verify_chunked_candidate(
    store: &LocalStore,
    chunks: &[craftcloud_db::versions::DownloadChunk],
    size_bytes: i64,
    sha256: &[u8],
) -> Result<(), ChunkVerificationError> {
    let (Ok(size), Ok(expected)) = (u64::try_from(size_bytes), <[u8; 32]>::try_from(sha256)) else {
        return Err(ChunkVerificationError::Integrity);
    };
    let mut reader = match crate::versions::ChunkedReader::new(
        store.clone(),
        chunks.to_vec(),
        size,
        0,
        size,
        None,
        None,
    )
    .await
    {
        Ok(reader) => reader,
        Err(crate::versions::ChunkOpenError::Integrity) => {
            return Err(ChunkVerificationError::Integrity);
        }
        Err(crate::versions::ChunkOpenError::Local(StorageError::NotFound)) => {
            return Err(ChunkVerificationError::Missing);
        }
        Err(_) => return Err(ChunkVerificationError::Storage),
    };
    let mut digest = Sha256::new();
    let mut read = 0u64;
    loop {
        match reader.next_chunk().await {
            Ok(Some(chunk)) => {
                read = read
                    .checked_add(chunk.len() as u64)
                    .ok_or(ChunkVerificationError::Integrity)?;
                digest.update(&chunk);
            }
            Ok(None) => break,
            Err(crate::versions::DownloadReadError::Chunked(StorageError::NotFound)) => {
                return Err(ChunkVerificationError::Missing);
            }
            Err(crate::versions::DownloadReadError::Chunked(
                StorageError::ChecksumMismatch
                | StorageError::LengthMismatch
                | StorageError::TooLarge,
            )) => return Err(ChunkVerificationError::Integrity),
            Err(_) => return Err(ChunkVerificationError::Storage),
        }
    }
    if read != size || <[u8; 32]>::from(digest.finalize()) != expected {
        return Err(ChunkVerificationError::Integrity);
    }
    Ok(())
}

/// Extra time on top of the upload TTL before an object or blob without
/// owner is considered abandoned.
const GRACE_EXTRA: Duration = Duration::from_secs(3600);

/// Largest accepted difference between the application clock and the database clock.
const DEFAULT_MAX_CLOCK_DRIFT: Duration = Duration::from_secs(300);

/// Reconciliation brake: it aborts when at least this many objects older than
/// the grace period are unknown ...
const BRAKE_MIN_UNKNOWN: usize = 100;
/// ... and they are more than one in `BRAKE_RATIO` of all objects older than the grace period.
const BRAKE_RATIO: usize = 10;
/// Small stores: the brake also applies when at least this many objects are
/// unknown and they are more than half of the objects older than the grace period.
const BRAKE_SMALL_MIN_UNKNOWN: usize = 10;

/// An item that failed is left alone for this long ...
const FAILED_ITEM_TTL: time::Duration = time::Duration::hours(24);
/// ... and at most this many failed items are remembered (oldest forgotten first).
const FAILED_ITEM_MAX: usize = 10_000;

/// Limits of a maintenance run.
#[derive(Debug, Clone)]
pub struct MaintenanceSettings {
    /// How long trashed projects and files stay (same value as the restore window).
    pub trash_retention: Duration,
    /// How long a superseded version stays after its successor was created.
    pub version_retention: Duration,
    /// Version thinning policy, overridden by instance settings unless locked by env.
    pub version_thinning: db::VersionThinningPolicy,
    /// Lifetime of an upload session; the grace period for blobs and stray
    /// objects is this plus one hour.
    pub upload_ttl: Duration,
    /// Most items a step handles per run (and per batch).
    pub batch_size: u32,
    /// Bytes the verification step re-reads per run (the first blob is always
    /// verified, so a blob larger than the budget is not skipped forever).
    pub verify_bytes_per_run: u64,
    /// Most storage entries the reconciliation lists per run.
    pub scan_limit: usize,
    /// Time budget of one run; the run stops between items when it is used up.
    pub max_run: Duration,
    /// How long quarantined objects are kept before they are deleted.
    pub quarantine_retention: Duration,
    /// Largest accepted difference between the application clock and the
    /// database clock; above it the destructive steps are skipped. `None`
    /// disables the check (tests with a manual clock).
    pub max_clock_drift: Option<Duration>,
    /// Organizations examined per run by the steps that walk them in windows.
    pub org_window: u32,
    /// Audit events older than this many days are deleted (ADR-0016: one
    /// year by default; `CRAFTCLOUD_AUDIT_RETENTION_DAYS`).
    pub audit_retention_days: u32,
}

impl MaintenanceSettings {
    /// Settings with the given retention windows and the default batch limits.
    #[must_use]
    pub fn new(
        trash_retention: Duration,
        version_retention: Duration,
        upload_ttl: Duration,
        verify_bytes_per_run: u64,
    ) -> Self {
        Self {
            trash_retention,
            version_retention,
            version_thinning: db::VersionThinningPolicy {
                retention_days: u32::try_from(version_retention.as_secs() / 86_400)
                    .unwrap_or(u32::MAX),
                keep_all_hours: 24,
                daily_days: 7,
                weekly_weeks: 4,
                max_per_file: 0,
            },
            upload_ttl,
            batch_size: 500,
            verify_bytes_per_run,
            scan_limit: 10_000,
            max_run: Duration::from_secs(120),
            quarantine_retention: Duration::from_secs(30 * 24 * 3600),
            max_clock_drift: Some(DEFAULT_MAX_CLOCK_DRIFT),
            org_window: 50,
            audit_retention_days: craftcloud_db::audit::DEFAULT_RETENTION_DAYS,
        }
    }

    fn grace(&self) -> Duration {
        self.upload_ttl.saturating_add(GRACE_EXTRA)
    }

    fn batch(&self) -> i64 {
        i64::from(self.batch_size.max(1))
    }
}

/// Who started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The loop inside the server.
    Scheduled,
    /// `craftcloud-server admin maintenance`.
    Cli,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Cli => "cli",
        }
    }
}

/// Options of one run.
#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    /// Count what would happen without changing anything (database or storage).
    pub dry_run: bool,
    /// Who started the run.
    pub trigger: Trigger,
}

/// Counts of one step and its error code, if it failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepReport {
    /// What the step did, by name.
    pub counts: BTreeMap<&'static str, u64>,
    /// `DATABASE_ERROR`, `STORAGE_ERROR`, `STORE_BINDING_MISMATCH`,
    /// `RECONCILE_BRAKE` or `CLOCK_DRIFT` if the step failed or was held back.
    pub error: Option<&'static str>,
}

struct FinishBlob<'a> {
    store: &'a LocalStore,
    blob_id: Uuid,
    object_key: &'a str,
    archive_object_key: Option<&'a str>,
    archive_delete_pending: bool,
    now: OffsetDateTime,
    report: &'a mut StepReport,
}

impl StepReport {
    fn add(&mut self, key: &'static str, amount: u64) {
        let slot = self.counts.entry(key).or_default();
        *slot = slot.saturating_add(amount);
    }
}

/// The result of a finished run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Per step, by name.
    pub steps: BTreeMap<&'static str, StepReport>,
}

impl RunReport {
    /// A count of a step (0 if absent).
    #[must_use]
    pub fn count(&self, step: &str, key: &str) -> u64 {
        self.steps
            .get(step)
            .and_then(|s| s.counts.get(key))
            .copied()
            .unwrap_or(0)
    }

    /// Whether any step failed or counted an error (`*_errors`).
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.steps.values().any(|s| {
            s.error.is_some()
                || s.counts
                    .iter()
                    .any(|(name, value)| name.ends_with("_errors") && *value > 0)
        })
    }

    /// Counts and error codes as JSON (the stored and published form).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let steps: serde_json::Map<String, Value> = self
            .steps
            .iter()
            .map(|(name, step)| {
                (
                    (*name).to_owned(),
                    json!({ "counts": step.counts, "error_code": step.error }),
                )
            })
            .collect();
        json!({ "dry_run": self.dry_run, "steps": steps })
    }
}

/// What [`Maintenance::run_once`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Another run holds the advisory lock; nothing was done.
    Skipped,
    /// The run finished.
    Completed(RunReport),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreState {
    /// No object store configured.
    Absent,
    /// Bound and in agreement with the database.
    Ready,
    /// Marker and database disagree: no storage step may touch anything.
    Mismatch,
    /// The binding could not be checked.
    Unavailable,
    /// The epochs show a restored database or marker: no destructive step may run.
    EpochConflict(EpochConflict),
}

struct Ctx<'a> {
    now: OffsetDateTime,
    dry: bool,
    deadline: tokio::time::Instant,
    stop: &'a watch::Receiver<bool>,
    /// The application clock and the database clock differ too much.
    drift: bool,
    store: StoreState,
    settings: MaintenanceSettings,
}

impl Ctx<'_> {
    /// Whether another item may be started.
    fn go(&self) -> bool {
        !*self.stop.borrow() && tokio::time::Instant::now() < self.deadline
    }
}

/// `now - span`, saturating at the epoch (so that nothing counts as old).
fn minus(now: OffsetDateTime, span: Duration) -> OffsetDateTime {
    time::Duration::try_from(span)
        .ok()
        .and_then(|span| now.checked_sub(span))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn count_of(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// `yyyymmdd` of a time (UTC), the name of a quarantine day directory.
fn day_name(time: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}",
        time.year(),
        u8::from(time.month()),
        time.day()
    )
}

/// Logs and counts an item that failed; the step goes on with the next item.
fn item_failed(report: &mut StepReport, what: &str, id: Uuid, error: &DbError) {
    tracing::error!(
        target: "craftcloud::maintenance",
        item = %id,
        %error,
        "{what} failed; continuing with the next item"
    );
    report.add("item_errors", 1);
}

/// Items that failed recently, per step. They are left out of the next
/// selections for a day so that a poisoned head of the queue cannot starve the
/// items behind it; the memory is bounded and lives in the process.
#[derive(Default)]
struct FailedItems {
    map: HashMap<(&'static str, Uuid), OffsetDateTime>,
    order: VecDeque<(&'static str, Uuid)>,
}

impl FailedItems {
    fn record(&mut self, step: &'static str, id: Uuid, now: OffsetDateTime) {
        if self.map.insert((step, id), now).is_none() {
            self.order.push_back((step, id));
        }
        while self.map.len() > FAILED_ITEM_MAX {
            match self.order.pop_front() {
                Some(key) => {
                    self.map.remove(&key);
                }
                None => break,
            }
        }
        // Forgotten entries leave stale keys in the queue; compact now and then.
        if self.order.len() > FAILED_ITEM_MAX.saturating_mul(2) {
            self.order.retain(|key| self.map.contains_key(key));
        }
    }

    fn active(&mut self, step: &'static str, now: OffsetDateTime) -> Vec<Uuid> {
        self.map.retain(|_, at| now - *at < FAILED_ITEM_TTL);
        self.map
            .keys()
            .filter(|(name, _)| *name == step)
            .map(|(_, id)| *id)
            .collect()
    }
}

// ---- store binding -------------------------------------------------------------

/// Why a store does not belong to the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingMismatch {
    /// The database is bound but the store directory has no marker.
    MarkerMissing,
    /// The store directory has a marker but the database is not bound.
    DatabaseUnbound,
    /// Both exist and name different stores.
    DifferentIds,
}

/// The epochs of database and marker disagree: one of them was restored from
/// an older state, so what the database says may be stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochConflict {
    /// The marker is ahead: the database is older than the store.
    DatabaseOlder,
    /// The database is more than one ahead of the marker: the marker is older.
    MarkerOlder,
}

impl EpochConflict {
    fn code(self) -> &'static str {
        match self {
            Self::DatabaseOlder => "DATABASE_OLDER_THAN_STORE",
            Self::MarkerOlder => "MARKER_OLDER_THAN_DATABASE",
        }
    }
}

/// Result of checking (or establishing) the pairing of database and store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// First use: marker and database binding were created.
    Created(Uuid),
    /// Marker and database agree.
    Matches(Uuid),
    /// They do not; the store must not be used.
    Mismatch(BindingMismatch),
    /// Same store, but the epochs show a restore: serving is fine, destructive
    /// maintenance must not run.
    EpochConflict(EpochConflict),
}

/// The binding could not be checked.
#[derive(Debug)]
pub enum BindingError {
    /// Database failure.
    Database(DbError),
    /// Storage failure (including an unreadable marker).
    Storage(StorageError),
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(f, "{error}"),
            Self::Storage(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for BindingError {}

impl From<DbError> for BindingError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

impl From<StorageError> for BindingError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Checks that `store` belongs to the database. When neither has an identity
/// yet (first use) and `create` is set, both are created; there is no other
/// automatic repair. Any other disagreement is reported as a mismatch.
///
/// # Errors
///
/// [`BindingError`] if the database or the marker cannot be read or written.
pub async fn check_store_binding(
    pool: &PgPool,
    store: &LocalStore,
    now: OffsetDateTime,
    create: bool,
) -> Result<Binding, BindingError> {
    let database = db::store_binding_epoch(pool).await?;
    let marker = store.read_marker_info().await?;
    match (database, marker) {
        (Some((id, epoch)), Some(info)) if id == info.store_id => {
            let epoch = u64::try_from(epoch).unwrap_or(0);
            if epoch == info.epoch {
                Ok(Binding::Matches(id))
            } else if epoch == info.epoch.saturating_add(1) {
                // The database was bumped, then the process died before the
                // marker followed: finish that step.
                if create {
                    store
                        .write_marker_info(
                            MarkerInfo {
                                store_id: id,
                                epoch,
                            },
                            true,
                        )
                        .await?;
                }
                Ok(Binding::Matches(id))
            } else if info.epoch > epoch {
                Ok(Binding::EpochConflict(EpochConflict::DatabaseOlder))
            } else {
                Ok(Binding::EpochConflict(EpochConflict::MarkerOlder))
            }
        }
        (Some(_), Some(_)) => Ok(Binding::Mismatch(BindingMismatch::DifferentIds)),
        (Some(_), None) => Ok(Binding::Mismatch(BindingMismatch::MarkerMissing)),
        (None, Some(_)) => Ok(Binding::Mismatch(BindingMismatch::DatabaseUnbound)),
        (None, None) => {
            let id = Uuid::new_v4();
            if !create {
                return Ok(Binding::Created(id));
            }
            match store
                .write_marker_info(
                    MarkerInfo {
                        store_id: id,
                        epoch: 0,
                    },
                    false,
                )
                .await
            {
                Ok(()) => {}
                // Another process is binding right now; check again next time.
                Err(StorageError::AlreadyExists) => {
                    return Ok(Binding::Mismatch(BindingMismatch::DatabaseUnbound));
                }
                Err(error) => return Err(error.into()),
            }
            if db::bind_store_if_unbound(pool, id, now).await? {
                Ok(Binding::Created(id))
            } else {
                Ok(Binding::Mismatch(BindingMismatch::DifferentIds))
            }
        }
    }
}

/// Checks a store for an operation that must never establish a new pairing.
/// An already-bound database is required before checking the marker, so an
/// empty foreign directory cannot be implicitly adopted by recall or another
/// maintenance command. A matching store may still finish the one-step marker
/// update left by a crash after an epoch bump.
pub async fn check_existing_store_binding(
    pool: &PgPool,
    store: &LocalStore,
    now: OffsetDateTime,
) -> Result<Binding, BindingError> {
    if db::store_binding_epoch(pool).await?.is_none() {
        return Ok(Binding::Mismatch(BindingMismatch::DatabaseUnbound));
    }
    check_store_binding(pool, store, now, true).await
}

/// What a forced rebinding found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rebound {
    /// The identity the database had, if any.
    pub old_database: Option<Uuid>,
    /// The identity in the store marker, if any.
    pub old_marker: Option<Uuid>,
    /// The identity both have now.
    pub new: Uuid,
    /// The epoch both have now (the marker's: the operator accepts the
    /// database as authoritative).
    pub epoch: u64,
}

/// Operator decision (`craftcloud-server admin storage-bind --force`): makes
/// database and store agree. The marker of the store wins (identity and
/// epoch); a store without a marker gets a new identity at epoch 0. This is
/// also how a restored older database is accepted. The caller must have
/// verified that this database and this store directory belong together.
///
/// # Errors
///
/// [`BindingError`] if the database or the marker cannot be read or written.
pub async fn rebind_store(
    pool: &PgPool,
    store: &LocalStore,
    now: OffsetDateTime,
) -> Result<Rebound, BindingError> {
    let marker = store.read_marker_info().await?;
    let (new, epoch) = match marker {
        Some(info) => (info.store_id, info.epoch),
        None => {
            let id = Uuid::new_v4();
            store
                .write_marker_info(
                    MarkerInfo {
                        store_id: id,
                        epoch: 0,
                    },
                    false,
                )
                .await?;
            (id, 0)
        }
    };
    let old_database =
        db::rebind_store(pool, new, i64::try_from(epoch).unwrap_or(i64::MAX), now).await?;
    Ok(Rebound {
        old_database,
        old_marker: marker.map(|info| info.store_id),
        new,
        epoch,
    })
}

/// The maintenance job. Cheap to share; hold it in an `Arc` for the loop.
pub struct Maintenance {
    pool: PgPool,
    store: Option<LocalStore>,
    archive_store: Option<Arc<craftcloud_storage::ArchiveStore>>,
    clock: Arc<dyn Clock>,
    settings: MaintenanceSettings,
    retention_environment: [bool; 4],
    version_setting_environment: [bool; 4],
    /// Where the reconciliation continues its walk through the object store.
    cursor: Mutex<Option<ObjectKey>>,
    /// Where the legacy retention and drift steps continue their walk through
    /// organizations.
    trim_cursor: Mutex<Option<Uuid>>,
    /// Where the current version-thinning scan continues its global file walk.
    thin_cursor: Mutex<Option<(Uuid, Uuid)>>,
    drift_cursor: Mutex<Option<Uuid>>,
    /// Items that failed recently (left out of the next selections).
    failed: Mutex<FailedItems>,
    /// Where finished runs are counted for the metrics endpoint.
    metrics: Option<Arc<crate::Metrics>>,
}

impl Maintenance {
    /// Creates the job. Without a store the storage steps are skipped.
    #[must_use]
    pub fn new(
        pool: PgPool,
        store: Option<LocalStore>,
        clock: Arc<dyn Clock>,
        settings: MaintenanceSettings,
    ) -> Self {
        Self {
            pool,
            store,
            archive_store: None,
            clock,
            settings,
            retention_environment: [false; 4],
            version_setting_environment: [false; 4],
            cursor: Mutex::new(None),
            trim_cursor: Mutex::new(None),
            thin_cursor: Mutex::new(None),
            drift_cursor: Mutex::new(None),
            failed: Mutex::new(FailedItems::default()),
            metrics: None,
        }
    }

    /// Counts finished runs (ok or failed) in `metrics`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<crate::Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Locks each retention value to its environment-provided server setting.
    #[must_use]
    pub fn with_retention_environment(mut self, locked: [bool; 4]) -> Self {
        self.retention_environment = locked;
        self
    }

    pub fn with_version_setting_environment(mut self, locked: [bool; 4]) -> Self {
        self.version_setting_environment = locked;
        self
    }

    /// Attaches the archive provider used by garbage collection to delete
    /// unpinned remote objects after their local copy is removed.
    #[must_use]
    pub fn with_archive(mut self, store: Arc<craftcloud_storage::ArchiveStore>) -> Self {
        self.archive_store = Some(store);
        self
    }

    /// Items of `step` that failed within the last day; counted in the report.
    fn excluded(
        &self,
        step: &'static str,
        now: OffsetDateTime,
        report: &mut StepReport,
    ) -> Vec<Uuid> {
        let ids = self
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active(step, now);
        if !ids.is_empty() {
            report.add("excluded_items", count_of(ids.len()));
        }
        ids
    }

    /// Remembers a failed item and logs it.
    fn fail_item(
        &self,
        ctx: &Ctx<'_>,
        step: &'static str,
        report: &mut StepReport,
        what: &str,
        id: Uuid,
        error: &DbError,
    ) {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(step, id, ctx.now);
        item_failed(report, what, id, error);
    }

    /// Runs the job every `interval` until `stop` turns true (or its sender
    /// is dropped). A run in progress stops between items.
    pub async fn run_loop(self: Arc<Self>, interval: Duration, mut stop: watch::Receiver<bool>) {
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                changed = stop.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
            if *stop.borrow() {
                break;
            }
            let options = RunOptions {
                dry_run: false,
                trigger: Trigger::Scheduled,
            };
            match self.run_once(options, &stop).await {
                Ok(RunOutcome::Completed(report)) => {
                    tracing::info!(
                        target: "craftcloud::maintenance",
                        report = %report.to_json(),
                        "maintenance run finished"
                    );
                }
                Ok(RunOutcome::Skipped) => {
                    tracing::info!(
                        target: "craftcloud::maintenance",
                        "maintenance run skipped: another run is active"
                    );
                }
                Err(error) => {
                    tracing::error!(target: "craftcloud::maintenance", %error, "maintenance run failed");
                }
            }
        }
        tracing::info!(target: "craftcloud::maintenance", "maintenance loop stopped");
    }

    /// Runs the job once, unless another run holds the lock.
    ///
    /// # Errors
    ///
    /// Database failures while taking the lock; failures inside a step are
    /// reported in the [`RunReport`].
    pub async fn run_once(
        &self,
        options: RunOptions,
        stop: &watch::Receiver<bool>,
    ) -> Result<RunOutcome, DbError> {
        // The run lock holds one connection for the whole run; the steps need others.
        if self.pool.options().get_max_connections() < 2 {
            tracing::error!(
                target: "craftcloud::maintenance",
                "maintenance needs a database pool of at least 2 connections"
            );
            return Err(DbError::Internal);
        }
        let Some(lock) = db::try_lock(&self.pool).await? else {
            return Ok(RunOutcome::Skipped);
        };
        let settings = self.effective_settings().await?;
        let started = tokio::time::Instant::now();
        let now = self.clock.now();
        let drift = match settings.max_clock_drift {
            None => false,
            Some(max) => {
                let database_now = db::db_now(&self.pool).await?;
                let difference = (now - database_now).unsigned_abs();
                let drifting = difference > max;
                if drifting {
                    tracing::error!(
                        target: "craftcloud::maintenance",
                        drift_secs = difference.as_secs(),
                        "the application clock differs from the database clock; destructive steps are skipped"
                    );
                }
                drifting
            }
        };
        let store = match &self.store {
            None => StoreState::Absent,
            Some(store) => {
                match check_store_binding(&self.pool, store, now, !options.dry_run).await {
                    Ok(Binding::Created(id)) => {
                        if !options.dry_run {
                            tracing::info!(target: "craftcloud::maintenance", store = %id, "object store bound to this database");
                        }
                        StoreState::Ready
                    }
                    Ok(Binding::Matches(_)) => StoreState::Ready,
                    Ok(Binding::EpochConflict(conflict)) => {
                        tracing::error!(
                            target: "craftcloud::maintenance",
                            ?conflict,
                            "the database and the object store are out of step (a restored database or marker?); \
                             destructive steps are skipped (verify the pairing, then run `craftcloud-server admin storage-bind --force`)"
                        );
                        StoreState::EpochConflict(conflict)
                    }
                    Ok(Binding::Mismatch(reason)) => {
                        tracing::error!(
                            target: "craftcloud::maintenance",
                            ?reason,
                            "the object store does not belong to this database; storage steps are skipped \
                             (verify the pairing, then run `craftcloud-server admin storage-bind --force`)"
                        );
                        StoreState::Mismatch
                    }
                    Err(error) => {
                        tracing::error!(target: "craftcloud::maintenance", %error, "cannot check the store binding");
                        StoreState::Unavailable
                    }
                }
            }
        };
        // A run that may delete something first raises the epoch in the
        // database, then copies it to the marker. A database restored from an
        // older state is then behind the marker and is noticed.
        let store = match (&self.store, store) {
            (Some(store_handle), StoreState::Ready) if !options.dry_run && !drift => {
                self.advance_epoch(store_handle).await
            }
            (_, state) => state,
        };
        if let (Some(store_handle), StoreState::Ready, false) =
            (&self.store, store, options.dry_run)
        {
            // Readiness probes that a crash left behind in `tmp`.
            if let Err(error) = store_handle
                .sweep_health_probes(Duration::from_secs(3600))
                .await
            {
                tracing::warn!(target: "craftcloud::maintenance", %error, "cannot sweep stale health probes");
            }
        }
        let ctx = Ctx {
            now,
            dry: options.dry_run,
            deadline: started + settings.max_run,
            stop,
            drift,
            store,
            settings,
        };
        let mut report = RunReport {
            dry_run: options.dry_run,
            steps: BTreeMap::new(),
        };
        for step in STEPS {
            let mut step_report = StepReport::default();
            if *ctx.stop.borrow() {
                step_report.add("skipped_shutdown", 1);
            } else if step.destructive() && ctx.drift {
                step_report.add("skipped_clock_drift", 1);
                step_report.error = Some("CLOCK_DRIFT");
            } else if let (true, StoreState::EpochConflict(conflict)) =
                (step.destructive(), ctx.store)
            {
                step_report.add("skipped_epoch_conflict", 1);
                step_report.error = Some(conflict.code());
            } else if step.needs_store() && ctx.store == StoreState::Mismatch {
                step_report.add("skipped_store_mismatch", 1);
                step_report.error = Some("STORE_BINDING_MISMATCH");
            } else if step.needs_store() && ctx.store == StoreState::Unavailable {
                step_report.add("skipped_store_unavailable", 1);
                step_report.error = Some("STORAGE_ERROR");
            } else if let Err(error) = self.run_step(step, &ctx, &mut step_report).await {
                tracing::error!(
                    target: "craftcloud::maintenance",
                    step = step.name(),
                    %error,
                    "maintenance step failed"
                );
                step_report.error = Some("DATABASE_ERROR");
            }
            if step_report.error.is_none() {
                let count = |key: &str| step_report.counts.get(key).copied().unwrap_or(0);
                if count("storage_errors") > 0 {
                    step_report.error = Some("STORAGE_ERROR");
                } else if count("item_errors") > 0 || count("db_errors") > 0 {
                    step_report.error = Some("DATABASE_ERROR");
                }
            }
            report.steps.insert(step.name(), step_report);
        }
        if !options.dry_run
            && let Some(metrics) = &self.metrics
        {
            metrics.maintenance_finished(!report.has_errors(), self.clock.now().unix_timestamp());
        }
        if !options.dry_run {
            let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            let recorded = db::record_run(
                &self.pool,
                options.trigger.as_str(),
                ctx.now,
                self.clock.now(),
                duration_ms,
                &report.to_json().to_string(),
            )
            .await;
            if let Err(error) = recorded {
                tracing::error!(target: "craftcloud::maintenance", %error, "cannot record the maintenance run");
            }
        }
        if let Err(error) = lock.release().await {
            tracing::warn!(target: "craftcloud::maintenance", %error, "cannot release the maintenance lock");
        }
        Ok(RunOutcome::Completed(report))
    }

    async fn effective_settings(&self) -> Result<MaintenanceSettings, DbError> {
        let mut settings = self.settings.clone();
        let stored = craftcloud_db::instance_settings::settings(&self.pool).await?;
        let now = self.clock.now();
        // Invalid stored values are ignored (logged by key) so that the
        // environment or default value keeps the job running. A recently
        // shortened period keeps its longer previous value for the grace time.
        let days = |key: &str| {
            craftcloud_db::instance_settings::retention_days_from_rows(&stored, key, now)
                .and_then(|value| u32::try_from(value).ok())
        };
        let seconds = |value: u32| Duration::from_secs(u64::from(value).saturating_mul(86_400));
        if !self.retention_environment[0]
            && let Some(value) = days("retention.version_days")
        {
            settings.version_retention = seconds(value);
        }
        if !self.retention_environment[1]
            && let Some(value) = days("retention.trash_days")
        {
            settings.trash_retention = seconds(value);
        }
        if !self.retention_environment[2]
            && let Some(value) = days("retention.audit_days")
        {
            settings.audit_retention_days = value;
        }
        if !self.retention_environment[3]
            && let Some(value) = days("retention.quarantine_days")
        {
            settings.quarantine_retention = seconds(value);
        }
        settings.version_thinning.retention_days =
            u32::try_from(settings.version_retention.as_secs() / 86_400).unwrap_or(u32::MAX);
        let keys = [
            "versions.keep_all_hours",
            "versions.daily_days",
            "versions.weekly_weeks",
            "versions.max_per_file",
        ];
        let mut values = [
            settings.version_thinning.keep_all_hours,
            settings.version_thinning.daily_days,
            settings.version_thinning.weekly_weeks,
            settings.version_thinning.max_per_file,
        ];
        for ((key, locked), current) in keys
            .iter()
            .zip(&self.version_setting_environment)
            .zip(&mut values)
        {
            if !locked
                && let Some(value) =
                    craftcloud_db::instance_settings::version_setting_from_rows(&stored, key, now)
                && let Ok(value) = u32::try_from(value)
            {
                *current = value;
            }
        }
        settings.version_thinning = db::VersionThinningPolicy {
            retention_days: settings.version_thinning.retention_days,
            keep_all_hours: values[0],
            daily_days: values[1],
            weekly_weeks: values[2],
            max_per_file: values[3],
        };
        Ok(settings)
    }

    /// Raises the epoch in the database, then in the marker (in this order: a
    /// crash in between leaves the database one ahead, which the next check repairs).
    async fn advance_epoch(&self, store: &LocalStore) -> StoreState {
        let info = match store.read_marker_info().await {
            Ok(Some(info)) => info,
            Ok(None) | Err(_) => return StoreState::Unavailable,
        };
        match db::bump_binding_epoch(&self.pool, info.store_id).await {
            Ok(Some(epoch)) => {
                let marker = MarkerInfo {
                    store_id: info.store_id,
                    epoch: u64::try_from(epoch).unwrap_or(0),
                };
                match store.write_marker_info(marker, true).await {
                    Ok(()) => StoreState::Ready,
                    Err(error) => {
                        tracing::error!(target: "craftcloud::maintenance", %error, "cannot update the store marker");
                        StoreState::Unavailable
                    }
                }
            }
            Ok(None) => StoreState::Mismatch,
            Err(error) => {
                tracing::error!(target: "craftcloud::maintenance", %error, "cannot raise the binding epoch");
                StoreState::Unavailable
            }
        }
    }

    async fn run_step(
        &self,
        step: Step,
        ctx: &Ctx<'_>,
        report: &mut StepReport,
    ) -> Result<(), DbError> {
        match step {
            Step::ExpireUploads => self.expire_uploads(ctx, report).await,
            Step::PurgeTrash => self.purge_trash(ctx, report).await,
            Step::TrimVersions => self.trim_versions(ctx, report).await,
            Step::CollectBlobs => self.collect_blobs(ctx, report).await,
            Step::ReconcileStorage => self.reconcile_storage(ctx, report).await,
            Step::VerifyBlobs => self.verify_blobs(ctx, report).await,
            Step::FixUsage => self.fix_usage(ctx, report).await,
            Step::PurgeAudit => self.purge_audit(ctx, report).await,
        }
    }

    // ---- 1. upload sessions -------------------------------------------------

    async fn expire_one(&self, org_id: Uuid, now: OffsetDateTime) -> Result<u64, DbError> {
        let Some(mut tx) = OrgTx::begin(&self.pool, org_id).await? else {
            return Ok(0);
        };
        let expired = tx.expire_due(now).await?;
        tx.commit().await?;
        Ok(expired)
    }

    async fn expire_uploads(&self, ctx: &Ctx<'_>, report: &mut StepReport) -> Result<(), DbError> {
        if ctx.dry {
            let due = db::count_due_sessions(&self.pool, ctx.now).await?;
            report.add("would_expire", u64::try_from(due).unwrap_or(0));
            return Ok(());
        }
        // Each organization handles up to 500 due sessions per round; an
        // organization that fails is skipped for the rest of the run.
        let excluded = self.excluded("expire_uploads", ctx.now, report);
        let mut failed: HashSet<Uuid> = HashSet::new();
        for _ in 0..10 {
            if !ctx.go() {
                break;
            }
            let orgs = db::orgs_with_due_sessions(&self.pool, ctx.now, &excluded, 1000).await?;
            let mut progress = 0;
            for org_id in orgs {
                if failed.contains(&org_id) {
                    continue;
                }
                if !ctx.go() {
                    break;
                }
                match self.expire_one(org_id, ctx.now).await {
                    Ok(expired) => {
                        report.add("expired_sessions", expired);
                        progress += expired;
                    }
                    Err(error) => {
                        self.fail_item(
                            ctx,
                            "expire_uploads",
                            report,
                            "expiring upload sessions",
                            org_id,
                            &error,
                        );
                        failed.insert(org_id);
                    }
                }
            }
            if progress == 0 {
                break;
            }
        }
        if ctx.dry {
            let due = craftcloud_db::locks::count_expired(&self.pool).await?;
            report.add("would_expire_file_locks", u64::try_from(due).unwrap_or(0));
        } else {
            let orgs = craftcloud_db::locks::orgs_with_expired(&self.pool, 1000).await?;
            for org_id in orgs {
                if !ctx.go() {
                    break;
                }
                match craftcloud_db::locks::expire_for_org(&self.pool, org_id, 1000).await {
                    Ok(expired) => report.add("expired_file_locks", expired),
                    Err(error) => self.fail_item(
                        ctx,
                        "expire_uploads",
                        report,
                        "expiring file locks",
                        org_id,
                        &error,
                    ),
                }
            }
        }
        Ok(())
    }

    // ---- 2. trash retention -------------------------------------------------

    async fn purge_file_item(
        &self,
        org_id: Uuid,
        file_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<Option<db::PurgeCounts>, DbError> {
        let Some(mut tx) = OrgTx::begin(&self.pool, org_id).await? else {
            return Ok(None);
        };
        let counts = tx.maint_purge_file(file_id, cutoff, now).await?;
        tx.commit().await?;
        Ok(Some(counts))
    }

    async fn purge_project_item(
        &self,
        org_id: Uuid,
        project_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<Option<db::PurgeCounts>, DbError> {
        let Some(mut tx) = OrgTx::begin(&self.pool, org_id).await? else {
            return Ok(None);
        };
        let counts = tx.maint_purge_project(project_id, cutoff, now, 100).await?;
        tx.commit().await?;
        Ok(Some(counts))
    }

    async fn purge_trash(&self, ctx: &Ctx<'_>, report: &mut StepReport) -> Result<(), DbError> {
        let cutoff = minus(ctx.now, ctx.settings.trash_retention);
        let batch = ctx.settings.batch();
        let excluded_files = self.excluded("purge_files", ctx.now, report);
        let excluded_projects = self.excluded("purge_projects", ctx.now, report);
        let files = db::trashed_files_due(&self.pool, cutoff, &excluded_files, batch).await?;
        let projects =
            db::trashed_projects_due(&self.pool, cutoff, &excluded_projects, batch).await?;
        if ctx.dry {
            report.add("files_due", count_of(files.len()));
            report.add("projects_due", count_of(projects.len()));
            return Ok(());
        }
        for (org_id, file_id) in files {
            if !ctx.go() {
                break;
            }
            match self.purge_file_item(org_id, file_id, cutoff, ctx.now).await {
                Ok(Some(counts)) => add_purged(report, counts),
                Ok(None) => {}
                Err(error) => self.fail_item(
                    ctx,
                    "purge_files",
                    report,
                    "purging a file",
                    file_id,
                    &error,
                ),
            }
        }
        for (org_id, project_id) in projects {
            // A project with many files is emptied over several short transactions.
            for _ in 0..50 {
                if !ctx.go() {
                    break;
                }
                match self
                    .purge_project_item(org_id, project_id, cutoff, ctx.now)
                    .await
                {
                    Ok(Some(counts)) => {
                        add_purged(report, counts);
                        if counts.projects == 1 || counts.files == 0 {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        self.fail_item(
                            ctx,
                            "purge_projects",
                            report,
                            "purging a project",
                            project_id,
                            &error,
                        );
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    // ---- 3. version retention -----------------------------------------------

    /// The next window of organizations for a step that walks them; `advance`
    /// moves the cursor (wrapping to the start after the last window).
    async fn next_orgs(
        &self,
        cursor: &Mutex<Option<Uuid>>,
        advance: bool,
    ) -> Result<Vec<Uuid>, DbError> {
        let after = *cursor.lock().unwrap_or_else(PoisonError::into_inner);
        let window = i64::from(self.settings.org_window.max(1));
        let orgs = db::org_window(&self.pool, after, window).await?;
        if advance {
            let next = if i64::try_from(orgs.len()).unwrap_or(i64::MAX) < window {
                None
            } else {
                orgs.last().copied()
            };
            *cursor.lock().unwrap_or_else(PoisonError::into_inner) = next;
        }
        Ok(orgs)
    }

    async fn trim_item(
        &self,
        org_id: Uuid,
        file_id: Uuid,
        policy: db::VersionThinningPolicy,
        now: OffsetDateTime,
        dry_run: bool,
    ) -> Result<u64, DbError> {
        let Some(mut tx) = OrgTx::begin(&self.pool, org_id).await? else {
            return Ok(0);
        };
        let deleted = tx
            .maint_trim_versions(file_id, policy, now, dry_run)
            .await?;
        tx.commit().await?;
        Ok(deleted)
    }

    async fn trim_versions(&self, ctx: &Ctx<'_>, report: &mut StepReport) -> Result<(), DbError> {
        let excluded = self.excluded("trim_versions", ctx.now, report);
        let policy = ctx.settings.version_thinning;
        let legacy = policy.keep_all_hours == 0
            && policy.daily_days == 0
            && policy.weekly_weeks == 0
            && policy.max_per_file == 0;
        let files = if legacy {
            let orgs = self.next_orgs(&self.trim_cursor, !ctx.dry).await?;
            db::files_with_old_versions(
                &self.pool,
                minus(ctx.now, ctx.settings.version_retention),
                &orgs,
                &excluded,
                ctx.settings.batch(),
            )
            .await?
        } else {
            let after = *self
                .thin_cursor
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let mut files =
                db::files_with_versions_to_thin(&self.pool, after, &excluded, ctx.settings.batch())
                    .await?;
            if files.is_empty() && after.is_some() {
                // A completed keyset pass wraps to the first file. Dry runs
                // use the wrapped page without changing the live cursor.
                files = db::files_with_versions_to_thin(
                    &self.pool,
                    None,
                    &excluded,
                    ctx.settings.batch(),
                )
                .await?;
                if !ctx.dry {
                    *self
                        .thin_cursor
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = None;
                }
            }
            files
        };
        for (org_id, file_id) in files {
            if !ctx.go() {
                break;
            }
            match self
                .trim_item(
                    org_id,
                    file_id,
                    ctx.settings.version_thinning,
                    ctx.now,
                    ctx.dry,
                )
                .await
            {
                Ok(0) => {}
                Ok(deleted) => {
                    report.add(
                        if ctx.dry {
                            "files_with_versions_to_thin"
                        } else {
                            "trimmed_files"
                        },
                        1,
                    );
                    report.add(
                        if ctx.dry {
                            "versions_thinned_due"
                        } else {
                            "deleted_versions"
                        },
                        deleted,
                    );
                }
                Err(error) => self.fail_item(
                    ctx,
                    "trim_versions",
                    report,
                    "trimming versions",
                    file_id,
                    &error,
                ),
            }
            if !legacy && !ctx.dry {
                *self
                    .thin_cursor
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some((org_id, file_id));
            }
        }
        Ok(())
    }

    // ---- 4. blob garbage collection -----------------------------------------

    async fn collect_blobs(&self, ctx: &Ctx<'_>, report: &mut StepReport) -> Result<(), DbError> {
        let Some(store) = &self.store else {
            report.add("skipped_no_storage", 1);
            return Ok(());
        };
        let created_before = minus(ctx.now, ctx.settings.grace());
        let batch = ctx.settings.batch();
        let excluded = self.excluded("collect_blobs", ctx.now, report);
        let deleting = db::gc_deleting_targets(&self.pool, batch).await?;
        let candidates = db::gc_candidates(&self.pool, created_before, &excluded, batch).await?;
        let deleting_chunks = db::gc_deleting_chunks(&self.pool, batch).await?;
        let chunk_candidates = db::gc_chunk_candidates(&self.pool, created_before, batch).await?;
        if ctx.dry {
            report.add("would_resume", count_of(deleting.len()));
            report.add("candidates", count_of(candidates.len()));
            report.add("chunk_would_resume", count_of(deleting_chunks.len()));
            report.add("chunk_candidates", count_of(chunk_candidates.len()));
            let retired = db::retired_archive_objects_due(
                &self.pool,
                minus(ctx.now, ctx.settings.grace()),
                ctx.now,
                batch,
            )
            .await?;
            report.add("retired_archive_candidates", count_of(retired.len()));
            return Ok(());
        }
        // Let one retired key advance before potentially long local GC batches;
        // the bounded tail drain below handles the rest when time remains.
        self.collect_retired_archive_objects(ctx, report, 1).await?;
        for target in deleting_chunks {
            if !ctx.go() {
                break;
            }
            self.finish_chunk(
                target.org_id,
                target.id,
                &target.object_key,
                target.archive_object_key.as_deref(),
                store,
                self.archive_store.as_deref(),
                report,
            )
            .await;
        }
        for (org_id, chunk_id) in chunk_candidates {
            if !ctx.go() {
                break;
            }
            let Some(claim) = self
                .claim_chunk_item(org_id, chunk_id, created_before)
                .await
                .unwrap_or_else(|error| {
                    self.fail_item(
                        ctx,
                        "collect_blobs",
                        report,
                        "claiming a chunk",
                        chunk_id,
                        &error,
                    );
                    None
                })
            else {
                report.add("chunk_skipped", 1);
                continue;
            };
            report.add("chunks_claimed", 1);
            self.finish_chunk(
                org_id,
                chunk_id,
                &claim.object_key,
                claim.archive_object_key.as_deref(),
                store,
                self.archive_store.as_deref(),
                report,
            )
            .await;
        }
        // Finish what an earlier run (or crash) left half done.
        for target in deleting {
            if !ctx.go() {
                break;
            }
            report.add("resumed", 1);
            self.finish_blob(FinishBlob {
                store,
                blob_id: target.id,
                object_key: &target.object_key,
                archive_object_key: target.archive_object_key.as_deref(),
                archive_delete_pending: target.archive_delete_pending,
                now: ctx.now,
                report,
            })
            .await;
        }
        for (org_id, blob_id) in candidates {
            if !ctx.go() {
                break;
            }
            let claim = match self.claim_item(org_id, blob_id, created_before).await {
                Ok(claim) => claim,
                Err(error) => {
                    self.fail_item(
                        ctx,
                        "collect_blobs",
                        report,
                        "claiming a blob",
                        blob_id,
                        &error,
                    );
                    continue;
                }
            };
            let GcClaim::Claimed(claimed) = claim else {
                report.add("skipped", 1);
                continue;
            };
            report.add("claimed", 1);
            if claimed.counted {
                report.add(
                    "released_bytes",
                    u64::try_from(claimed.size_bytes).unwrap_or(0),
                );
            }
            if claimed.clamped {
                tracing::warn!(
                    target: "craftcloud::maintenance",
                    org = %org_id,
                    blob = %blob_id,
                    "a usage counter was smaller than the blob; clamped at zero"
                );
                report.add("counter_clamped", 1);
            }
            self.finish_blob(FinishBlob {
                store,
                blob_id,
                object_key: &claimed.object_key,
                archive_object_key: claimed.archive_object_key.as_deref(),
                archive_delete_pending: claimed.archive_object_key.is_some(),
                now: ctx.now,
                report,
            })
            .await;
        }
        self.collect_retired_archive_objects(ctx, report, batch)
            .await?;
        Ok(())
    }

    async fn collect_retired_archive_objects(
        &self,
        ctx: &Ctx<'_>,
        report: &mut StepReport,
        limit: i64,
    ) -> Result<(), DbError> {
        let Some(archive) = &self.archive_store else {
            report.add("retired_archive_skipped_no_provider", 1);
            return Ok(());
        };
        // Inventory is checked in the DB query. The shared maintenance lock
        // serializes this drain with backup pin registration and release.
        let candidates = db::retired_archive_objects_due(
            &self.pool,
            minus(ctx.now, ctx.settings.grace()),
            ctx.now,
            limit,
        )
        .await?;
        for item in candidates {
            if !ctx.go() {
                break;
            }
            let Ok(key) = ObjectKey::parse(&item.archive_object_key) else {
                db::retired_archive_object_failed(
                    &self.pool,
                    &item.archive_object_key,
                    ctx.now
                        .checked_add(time::Duration::minutes(5))
                        .unwrap_or(ctx.now),
                    "INVALID_OBJECT_KEY",
                )
                .await?;
                report.add("retired_archive_errors", 1);
                continue;
            };
            match archive.delete(&key).await {
                Ok(()) | Err(StorageError::NotFound) => {}
                Err(error) => {
                    tracing::warn!(target: "craftcloud::maintenance", org = %item.org_id, %error, "retired archive object deletion failed; cleanup ownership retained");
                    db::retired_archive_object_failed(
                        &self.pool,
                        &item.archive_object_key,
                        ctx.now
                            .checked_add(time::Duration::minutes(5))
                            .unwrap_or(ctx.now),
                        "REMOTE_DELETE_FAILED",
                    )
                    .await?;
                    report.add("retired_archive_errors", 1);
                    continue;
                }
            }
            if db::retired_archive_object_finish(&self.pool, &item.archive_object_key).await? {
                report.add("retired_archive_objects_deleted", 1);
                report.add(
                    "retired_archive_bytes_deleted",
                    u64::try_from(item.physical_bytes).unwrap_or(0),
                );
            } else {
                // A defensive reference recheck keeps the queue row if a
                // reference appeared after candidate selection.
                report.add("retired_archive_referenced", 1);
            }
        }
        Ok(())
    }

    async fn claim_chunk_item(
        &self,
        org_id: Uuid,
        chunk_id: Uuid,
        created_before: OffsetDateTime,
    ) -> Result<Option<db::ChunkGcClaim>, DbError> {
        let Some(mut tx) = OrgTx::begin(&self.pool, org_id).await? else {
            return Ok(None);
        };
        let claim = tx.maint_chunk_gc_claim(chunk_id, created_before).await?;
        tx.commit().await?;
        Ok(claim)
    }

    // Chunk cleanup needs the same explicit claim data and storage providers
    // as finish_blob; retain this narrow lifecycle boundary.
    #[allow(clippy::too_many_arguments)]
    async fn finish_chunk(
        &self,
        org_id: Uuid,
        chunk_id: Uuid,
        object_key: &str,
        archive_object_key: Option<&str>,
        store: &LocalStore,
        archive: Option<&ArchiveStore>,
        report: &mut StepReport,
    ) {
        craftcloud_storage::crash_point("gc-chunk-after-claim", 0);
        let Ok(key) = ObjectKey::parse(object_key) else {
            tracing::error!(target: "craftcloud::maintenance", chunk = %chunk_id, "chunk has an invalid object key");
            report.add("chunk_invalid_records", 1);
            return;
        };
        match store.delete(&key).await {
            Ok(()) => report.add("chunk_objects_deleted", 1),
            Err(StorageError::NotFound) => report.add("chunk_objects_already_gone", 1),
            Err(error) => {
                tracing::warn!(target: "craftcloud::maintenance", chunk = %chunk_id, %error, "chunk object deletion failed; claim stays for retry");
                report.add("chunk_storage_errors", 1);
                return;
            }
        }
        if let Some(raw_archive_key) = archive_object_key {
            let Some(archive) = archive else {
                report.add("chunk_archive_unavailable", 1);
                return;
            };
            let Ok(archive_key) = ObjectKey::parse(raw_archive_key) else {
                report.add("chunk_invalid_records", 1);
                return;
            };
            match archive.delete(&archive_key).await {
                Ok(()) | Err(StorageError::NotFound) => {}
                Err(error) => {
                    tracing::warn!(target: "craftcloud::maintenance", chunk = %chunk_id, %error, "archived chunk deletion failed; claim stays for retry");
                    report.add("chunk_archive_errors", 1);
                    return;
                }
            }
        }
        let result: Result<bool, craftcloud_db::DbError> = async {
            let Some(mut tx) = OrgTx::begin(&self.pool, org_id).await? else {
                return Ok(false);
            };
            let removed = tx.maint_chunk_gc_finish(chunk_id).await?;
            tx.commit().await?;
            Ok(removed)
        }
        .await;
        match result {
            Ok(true) => report.add("chunks_deleted", 1),
            Ok(false) => report.add("chunk_skipped", 1),
            Err(_) => report.add("chunk_db_errors", 1),
        }
    }

    async fn claim_item(
        &self,
        org_id: Uuid,
        blob_id: Uuid,
        created_before: OffsetDateTime,
    ) -> Result<GcClaim, DbError> {
        let Some(mut tx) = OrgTx::begin(&self.pool, org_id).await? else {
            return Ok(GcClaim::Skipped);
        };
        let claim = tx.maint_gc_claim(blob_id, created_before).await?;
        tx.commit().await?;
        Ok(claim)
    }

    /// Phase 2: deletes the object, then the row. A failure leaves the blob
    /// `deleting`; the next run retries (on Windows an object that a download
    /// still holds open cannot be deleted).
    async fn finish_blob(&self, item: FinishBlob<'_>) {
        let FinishBlob {
            store,
            blob_id,
            object_key,
            archive_object_key,
            archive_delete_pending,
            now,
            report,
        } = item;
        // Crash test hook: the claim is committed, the object not deleted yet.
        craftcloud_storage::crash_point("gc-after-claim", 0);
        let Ok(key) = ObjectKey::parse(object_key) else {
            tracing::error!(target: "craftcloud::maintenance", blob = %blob_id, "blob has an invalid object key");
            report.add("storage_errors", 1);
            return;
        };
        match store.delete(&key).await {
            Ok(()) => report.add("objects_deleted", 1),
            Err(StorageError::NotFound) => report.add("objects_already_gone", 1),
            Err(error) => {
                tracing::warn!(
                    target: "craftcloud::maintenance",
                    blob = %blob_id,
                    %error,
                    "object could not be deleted; the blob stays in state deleting and is retried"
                );
                report.add("storage_errors", 1);
                return;
            }
        }
        if archive_delete_pending {
            let Some(archive) = &self.archive_store else {
                tracing::error!(target: "craftcloud::maintenance", blob = %blob_id, "archive object deletion is pending but no archive provider is configured");
                report.add("archive_unavailable", 1);
                return;
            };
            let Some(archive_object_key) = archive_object_key else {
                tracing::error!(target: "craftcloud::maintenance", blob = %blob_id, "archive deletion is pending without an object key");
                report.add("archive_errors", 1);
                return;
            };
            let Ok(archive_key) = ObjectKey::parse(archive_object_key) else {
                tracing::error!(target: "craftcloud::maintenance", blob = %blob_id, "archive object key is invalid");
                report.add("archive_errors", 1);
                return;
            };
            let delete_result = archive.delete(&archive_key).await;
            if delete_result.is_err() && !matches!(delete_result, Err(StorageError::NotFound)) {
                let retry_at = now.checked_add(time::Duration::minutes(5)).unwrap_or(now);
                match db::gc_archive_delete_failed(
                    &self.pool,
                    blob_id,
                    "REMOTE_DELETE_FAILED",
                    retry_at,
                )
                .await
                {
                    Ok(true) => report.add("archive_delete_retries", 1),
                    Ok(false) => report.add("archive_errors", 1),
                    Err(_) => report.add("db_errors", 1),
                }
                return;
            }
            match db::gc_archive_delete_finish(&self.pool, blob_id).await {
                Ok(true) => report.add("archive_objects_deleted", 1),
                Ok(false) => {
                    report.add("archive_errors", 1);
                    return;
                }
                Err(_) => {
                    report.add("db_errors", 1);
                    return;
                }
            }
        }
        match db::gc_finish(&self.pool, blob_id).await {
            Ok(true) => report.add("blobs_deleted", 1),
            Ok(false) => {}
            Err(error) => {
                tracing::error!(target: "craftcloud::maintenance", blob = %blob_id, %error, "cannot delete the blob row");
                report.add("db_errors", 1);
            }
        }
    }

    // ---- 5. reconciliation ---------------------------------------------------

    fn take_cursor(&self) -> Option<ObjectKey> {
        self.cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn put_cursor(&self, cursor: Option<ObjectKey>) {
        *self.cursor.lock().unwrap_or_else(PoisonError::into_inner) = cursor;
    }

    async fn reconcile_storage(
        &self,
        ctx: &Ctx<'_>,
        report: &mut StepReport,
    ) -> Result<(), DbError> {
        let Some(store) = &self.store else {
            report.add("skipped_no_storage", 1);
            return Ok(());
        };
        let old_before = minus(ctx.now, ctx.settings.grace());
        let is_old = |modified: Option<SystemTime>| {
            modified.is_some_and(|time| OffsetDateTime::from(time) < old_before)
        };
        let page = 500;

        // Phase A: find the objects without a blob row (nothing is moved yet).
        // The walk continues where the last run stopped and wraps around at the
        // end of the store.
        let original = self.take_cursor();
        let mut cursor = original.clone();
        let mut scanned = 0_usize;
        let mut old_total = 0_usize;
        let mut strays: Vec<ObjectKey> = Vec::new();
        let mut failure = None;
        while ctx.go() && scanned < ctx.settings.scan_limit {
            let items = match store.list_objects(cursor.as_ref(), page).await {
                Ok(items) => items,
                Err(error) => {
                    tracing::warn!(target: "craftcloud::maintenance", %error, "cannot list stored objects");
                    report.add("storage_errors", 1);
                    break;
                }
            };
            if items.is_empty() {
                cursor = None;
                break;
            }
            scanned += items.len();
            report.add("scanned_objects", count_of(items.len()));
            let old: Vec<&ObjectInfo> = items.iter().filter(|o| is_old(o.modified)).collect();
            old_total += old.len();
            let keys: Vec<String> = old.iter().map(|o| o.key.as_str().to_owned()).collect();
            let known = if keys.is_empty() {
                HashSet::new()
            } else {
                match db::known_object_keys(&self.pool, &keys).await {
                    Ok(known) => known,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            };
            strays.extend(
                old.iter()
                    .filter(|o| !known.contains(o.key.as_str()))
                    .map(|o| o.key.clone()),
            );
            let last = items.last().map(|o| o.key.clone());
            if items.len() < page {
                cursor = None;
                break;
            }
            cursor = last;
        }
        if let Some(error) = failure {
            self.put_cursor(original);
            return Err(error);
        }
        // The brake: a database that does not know a large share of the old
        // objects is much more likely the wrong database than a pile of strays.
        let unknown = strays.len();
        let braking = (unknown >= BRAKE_MIN_UNKNOWN && unknown * BRAKE_RATIO > old_total)
            || (unknown >= BRAKE_SMALL_MIN_UNKNOWN && unknown * 2 > old_total);
        if braking {
            tracing::warn!(
                target: "craftcloud::maintenance",
                unknown = strays.len(),
                old_objects = old_total,
                "reconciliation brake: too many objects are unknown to the database; nothing was moved"
            );
            report.add("unknown_objects", count_of(strays.len()));
            report.error = Some("RECONCILE_BRAKE");
            self.put_cursor(original);
            return Ok(());
        }
        self.put_cursor(if ctx.dry { original } else { cursor });

        // Phase B: quarantine the strays, purge old quarantine days.
        let day = day_name(ctx.now);
        if !ctx.dry && !strays.is_empty() {
            tracing::warn!(
                target: "craftcloud::maintenance",
                count = strays.len(),
                "moving objects that the database does not know to the quarantine"
            );
        }
        for key in &strays {
            if ctx.dry {
                report.add("would_quarantine_objects", 1);
                continue;
            }
            if !ctx.go() {
                break;
            }
            match store.quarantine(key, &day).await {
                Ok(()) => report.add("quarantined_objects", 1),
                Err(StorageError::NotFound) => {}
                Err(error) => {
                    tracing::warn!(target: "craftcloud::maintenance", %error, "cannot quarantine a stray object");
                    report.add("storage_errors", 1);
                }
            }
        }
        let quarantined = report
            .counts
            .get("quarantined_objects")
            .copied()
            .unwrap_or(0);
        if quarantined > 0 {
            // One summary event per run (the moves are not database changes).
            let event = craftcloud_db::audit::NewEvent::new("maintenance.quarantine").detail(
                "objects",
                craftcloud_db::audit::Detail::Count(i64::try_from(quarantined).unwrap_or(i64::MAX)),
            );
            crate::audit::record_inline(
                &self.pool,
                &craftcloud_db::audit::AuditCtx::system(),
                &event,
            )
            .await;
        }
        let purge_before = day_name(minus(ctx.now, ctx.settings.quarantine_retention));
        match store.purge_quarantine(&purge_before, ctx.dry).await {
            Ok(0) => {}
            Ok(removed) => report.add(
                if ctx.dry {
                    "would_purge_quarantine"
                } else {
                    "purged_quarantine"
                },
                removed,
            ),
            Err(error) => {
                tracing::warn!(target: "craftcloud::maintenance", %error, "cannot purge the quarantine");
                report.add("storage_errors", 1);
            }
        }

        // Temporary files of uploads that died.
        let mut after: Option<String> = None;
        let mut scanned = 0_usize;
        while ctx.go() && scanned < ctx.settings.scan_limit {
            let items = match store.list_temp(after.as_deref(), page).await {
                Ok(items) => items,
                Err(error) => {
                    tracing::warn!(target: "craftcloud::maintenance", %error, "cannot list temporary objects");
                    report.add("storage_errors", 1);
                    break;
                }
            };
            scanned += items.len();
            for temp in &items {
                if !is_old(temp.modified) {
                    continue;
                }
                if ctx.dry {
                    report.add("would_remove_temp", 1);
                    continue;
                }
                match store.remove_temp(&temp.id).await {
                    Ok(()) => report.add("removed_temp", 1),
                    Err(StorageError::NotFound) => {}
                    Err(error) => {
                        tracing::warn!(target: "craftcloud::maintenance", %error, "cannot remove a temporary object");
                        report.add("storage_errors", 1);
                    }
                }
            }
            if items.len() < page {
                break;
            }
            after = items.last().map(|t| t.id.clone());
        }
        Ok(())
    }

    // ---- 8. audit retention ----------------------------------------------------

    /// Deletes audit events older than the retention in bounded batches. The
    /// cutoff is computed by the database clock inside `audit_purge`, so this
    /// step does not depend on the application clock and is not held back by
    /// a clock drift or an epoch conflict: it deletes no stored object, and a
    /// restored older database only ever loses events that were due anyway.
    async fn purge_audit(&self, ctx: &Ctx<'_>, report: &mut StepReport) -> Result<(), DbError> {
        let days = ctx.settings.audit_retention_days;
        if ctx.dry {
            let due = craftcloud_db::audit::count_expired(&self.pool, days).await?;
            report.add("events_due", due);
            report.add(
                "device_sessions_due",
                craftcloud_db::device_sessions::count_expired(&self.pool).await?,
            );
            return Ok(());
        }
        let batch = ctx.settings.batch();
        for _ in 0..20 {
            if !ctx.go() {
                break;
            }
            let removed = craftcloud_db::audit::purge(&self.pool, days, batch).await?;
            report.add("purged_events", removed);
            if i64::try_from(removed).unwrap_or(i64::MAX) < batch {
                break;
            }
        }
        for _ in 0..20 {
            if !ctx.go() {
                break;
            }
            let removed = craftcloud_db::device_sessions::delete_expired(&self.pool, batch).await?;
            report.add("purged_device_sessions", removed);
            if i64::try_from(removed).unwrap_or(i64::MAX) < batch {
                break;
            }
        }
        if ctx.go() {
            report.add(
                "purged_emails",
                craftcloud_db::email_outbox::purge_terminal(&self.pool).await?,
            );
            report.add(
                "purged_email_tokens",
                craftcloud_db::email_verification::delete_expired(&self.pool).await?,
            );
        }
        Ok(())
    }

    // ---- 6. integrity verification -------------------------------------------

    async fn verify_blobs(&self, ctx: &Ctx<'_>, report: &mut StepReport) -> Result<(), DbError> {
        let Some(store) = &self.store else {
            report.add("skipped_no_storage", 1);
            return Ok(());
        };
        let candidates = db::verify_candidates(&self.pool, ctx.settings.batch()).await?;
        let mut bytes = 0_u64;
        for (handled, blob) in candidates.into_iter().enumerate() {
            if !ctx.go() {
                break;
            }
            // The first blob is always verified, whatever its size.
            if handled > 0 && bytes >= ctx.settings.verify_bytes_per_run {
                break;
            }
            let size = u64::try_from(blob.size_bytes).unwrap_or(0);
            bytes = bytes.saturating_add(size);
            if ctx.dry {
                report.add("would_verify", 1);
                continue;
            }
            let result = if blob.layout == "chunked" {
                let (Some(file_id), Some(version_id)) = (blob.file_id, blob.version_id) else {
                    tracing::error!(target: "craftcloud::maintenance", blob = %blob.id, "chunked blob has no version mapping");
                    report.add("invalid_records", 1);
                    continue;
                };
                let chunks =
                    match db::verify_candidate_chunks(&self.pool, blob.id, file_id, version_id)
                        .await
                    {
                        Ok(chunks) => chunks,
                        Err(error) => {
                            self.fail_item(
                                ctx,
                                "verify_blobs",
                                report,
                                "loading a chunk manifest",
                                blob.id,
                                &error,
                            );
                            continue;
                        }
                    };
                match verify_chunked_candidate(store, &chunks, blob.size_bytes, &blob.sha256).await
                {
                    Ok(()) => VerifyResult::Intact,
                    Err(ChunkVerificationError::Missing) => VerifyResult::Missing,
                    Err(ChunkVerificationError::Integrity) => VerifyResult::Mismatch,
                    Err(ChunkVerificationError::Storage) => {
                        tracing::warn!(target: "craftcloud::maintenance", blob = %blob.id, "chunked blob could not be read for verification");
                        report.add("storage_errors", 1);
                        continue;
                    }
                }
            } else {
                let (Ok(key), Ok(sha)) = (
                    ObjectKey::parse(&blob.object_key),
                    <[u8; 32]>::try_from(blob.sha256.as_slice()),
                ) else {
                    tracing::error!(target: "craftcloud::maintenance", blob = %blob.id, "blob record is malformed");
                    report.add("invalid_records", 1);
                    continue;
                };
                match store.verify(&key, &sha).await {
                    Ok(()) => VerifyResult::Intact,
                    Err(StorageError::ChecksumMismatch) => VerifyResult::Mismatch,
                    Err(StorageError::NotFound) => VerifyResult::Missing,
                    Err(error) => {
                        tracing::warn!(target: "craftcloud::maintenance", blob = %blob.id, %error, "blob could not be read for verification");
                        report.add("storage_errors", 1);
                        continue;
                    }
                }
            };
            let recorded = match db::record_verification(&self.pool, blob.id, result, ctx.now).await
            {
                Ok(recorded) => recorded,
                Err(error) => {
                    self.fail_item(
                        ctx,
                        "verify_blobs",
                        report,
                        "recording a verification",
                        blob.id,
                        &error,
                    );
                    continue;
                }
            };
            if recorded.is_none() {
                // Claimed by garbage collection meanwhile.
                continue;
            }
            let was_corrupt = blob.state == "corrupt";
            match result {
                VerifyResult::Intact => {
                    report.add("verified_bytes", size);
                    report.add(if was_corrupt { "repaired" } else { "verified" }, 1);
                    if was_corrupt {
                        tracing::info!(target: "craftcloud::maintenance", blob = %blob.id, "blob matches its checksum again");
                    }
                }
                VerifyResult::Mismatch => {
                    report.add("verified_bytes", size);
                    if was_corrupt {
                        report.add("still_corrupt", 1);
                    } else {
                        tracing::error!(target: "craftcloud::maintenance", blob = %blob.id, "blob content differs from its checksum; marked corrupt");
                        report.add("newly_corrupt", 1);
                    }
                }
                VerifyResult::Missing => {
                    tracing::error!(target: "craftcloud::maintenance", blob = %blob.id, state = %blob.state, "object of a stored blob is missing; state unchanged");
                    report.add("missing_objects", 1);
                }
            }
        }
        Ok(())
    }

    // ---- 7. usage drift ------------------------------------------------------

    async fn fix_one(&self, org_id: Uuid) -> Result<Option<db::UsageDrift>, DbError> {
        let Some(mut tx) = OrgTx::begin(&self.pool, org_id).await? else {
            return Ok(None);
        };
        let drift = tx.maint_fix_usage().await?;
        tx.commit().await?;
        Ok(drift)
    }

    async fn fix_usage(&self, ctx: &Ctx<'_>, report: &mut StepReport) -> Result<(), DbError> {
        let excluded = self.excluded("fix_usage", ctx.now, report);
        let window = self.next_orgs(&self.drift_cursor, !ctx.dry).await?;
        let orgs = db::drifting_orgs(
            &self.pool,
            &window,
            &excluded,
            ctx.settings.batch().min(100),
        )
        .await?;
        if ctx.dry {
            report.add("orgs_with_drift", count_of(orgs.len()));
            return Ok(());
        }
        for org_id in orgs {
            if !ctx.go() {
                break;
            }
            match self.fix_one(org_id).await {
                Ok(Some(drift)) => {
                    tracing::warn!(
                        target: "craftcloud::maintenance",
                        org = %org_id,
                        reserved_delta = drift.reserved_delta,
                        committed_delta = drift.committed_delta,
                        workspaces = drift.workspaces,
                        "usage counters differed from the rows and were corrected"
                    );
                    report.add("orgs_corrected", 1);
                }
                Ok(None) => {}
                Err(error) => self.fail_item(
                    ctx,
                    "fix_usage",
                    report,
                    "correcting usage counters",
                    org_id,
                    &error,
                ),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod setup_retention_tests {
    use super::*;
    use crate::app_access_tests::support::TestEnv;
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn persisted_trash_retention_applies_unless_environment_locks_it() {
        let env = TestEnv::new().await;
        sqlx::query("INSERT INTO instance_settings(key,value) VALUES('retention.trash_days', $1)")
            .bind(serde_json::json!({"value": 7}))
            .execute(&env.pool)
            .await
            .unwrap();
        let baseline = Duration::from_secs(30 * 86_400);
        let settings =
            MaintenanceSettings::new(baseline, baseline, Duration::from_secs(3600), 1024 * 1024);
        let dynamic = Maintenance::new(
            env.pool.clone(),
            None,
            Arc::new(crate::SystemClock),
            settings.clone(),
        )
        .effective_settings()
        .await
        .unwrap();
        assert_eq!(dynamic.trash_retention, Duration::from_secs(7 * 86_400));

        let locked = Maintenance::new(
            env.pool.clone(),
            None,
            Arc::new(crate::SystemClock),
            settings,
        )
        .with_retention_environment([false, true, false, false])
        .effective_settings()
        .await
        .unwrap();
        assert_eq!(locked.trash_retention, baseline);
    }

    async fn effective(env: &TestEnv, locked: [bool; 4]) -> MaintenanceSettings {
        let baseline = Duration::from_secs(30 * 86_400);
        let settings =
            MaintenanceSettings::new(baseline, baseline, Duration::from_secs(3600), 1024 * 1024);
        Maintenance::new(
            env.pool.clone(),
            None,
            Arc::new(crate::SystemClock),
            settings,
        )
        .with_retention_environment(locked)
        .effective_settings()
        .await
        .unwrap()
    }

    async fn store_retention(env: &TestEnv, key: &str, value: serde_json::Value, age_hours: i32) {
        sqlx::query(
            "INSERT INTO instance_settings(key,value,updated_at) \
             VALUES($1,$2, now() - make_interval(hours => $3)) \
             ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, updated_at=EXCLUDED.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(age_hours)
        .execute(&env.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn shortened_retention_keeps_the_longer_period_for_24_hours() {
        let env = TestEnv::new().await;
        let day = |days: u64| Duration::from_secs(days * 86_400);
        // Inside the grace period purge, trim and audit cleanup keep using the
        // old, longer periods.
        for (key, new, previous) in [
            ("retention.trash_days", 7, 60),
            ("retention.version_days", 3, 45),
            ("retention.quarantine_days", 2, 20),
            ("retention.audit_days", 31, 400),
        ] {
            store_retention(&env, key, json!({"value": new, "previous": previous}), 23).await;
        }
        store_retention(
            &env,
            "versions.keep_all_hours",
            json!({"value": 2, "previous": 24}),
            23,
        )
        .await;
        store_retention(
            &env,
            "versions.max_per_file",
            json!({"value": 100, "previous": 0}),
            23,
        )
        .await;
        let graced = effective(&env, [false; 4]).await;
        assert_eq!(graced.trash_retention, day(60));
        assert_eq!(graced.version_retention, day(45));
        assert_eq!(graced.quarantine_retention, day(20));
        assert_eq!(graced.audit_retention_days, 400);
        assert_eq!(graced.version_thinning.keep_all_hours, 24);
        assert_eq!(graced.version_thinning.max_per_file, 0);

        // After 24 hours the new, shorter periods apply.
        for (key, new, previous) in [
            ("retention.trash_days", 7, 60),
            ("retention.version_days", 3, 45),
            ("retention.quarantine_days", 2, 20),
            ("retention.audit_days", 31, 400),
        ] {
            store_retention(&env, key, json!({"value": new, "previous": previous}), 25).await;
        }
        store_retention(
            &env,
            "versions.keep_all_hours",
            json!({"value": 2, "previous": 24}),
            25,
        )
        .await;
        store_retention(
            &env,
            "versions.max_per_file",
            json!({"value": 100, "previous": 0}),
            25,
        )
        .await;
        let expired = effective(&env, [false; 4]).await;
        assert_eq!(expired.trash_retention, day(7));
        assert_eq!(expired.version_retention, day(3));
        assert_eq!(expired.quarantine_retention, day(2));
        assert_eq!(expired.audit_retention_days, 31);
        assert_eq!(expired.version_thinning.keep_all_hours, 2);
        assert_eq!(expired.version_thinning.max_per_file, 100);

        // Environment-locked values never take a stored grace value.
        let locked = effective(&env, [true, true, true, true]).await;
        assert_eq!(locked.trash_retention, day(30));
        // Let the test runtime return pooled connections before the synchronous
        // fixture destructor joins its separate cleanup runtime.
        env.pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn invalid_stored_retention_is_ignored_and_does_not_stop_maintenance() {
        let env = TestEnv::new().await;
        // The database constraint rejects invalid periods, so lift it for
        // this test to model rows written by an older or damaged installation.
        sqlx::query(
            "ALTER TABLE instance_settings DROP CONSTRAINT instance_settings_retention_days",
        )
        .execute(&env.pool)
        .await
        .unwrap();
        for (key, value) in [
            ("retention.trash_days", json!({"value": 0})),
            ("retention.version_days", json!({"value": "seven"})),
            ("retention.quarantine_days", json!({"value": 99999})),
            ("retention.audit_days", json!({"value": 29})),
        ] {
            store_retention(&env, key, value, 100).await;
        }
        let settings = effective(&env, [false; 4]).await;
        let baseline = Duration::from_secs(30 * 86_400);
        assert_eq!(settings.trash_retention, baseline);
        assert_eq!(settings.version_retention, baseline);
        assert_eq!(settings.quarantine_retention, baseline);
        assert_eq!(
            settings.audit_retention_days,
            craftcloud_db::audit::DEFAULT_RETENTION_DAYS
        );
    }
}

fn add_purged(report: &mut StepReport, counts: db::PurgeCounts) {
    report.add("purged_files", counts.files);
    report.add("purged_versions", counts.versions);
    report.add("purged_sessions", counts.sessions);
    report.add("purged_projects", counts.projects);
}

#[derive(Clone, Copy)]
enum Step {
    ExpireUploads,
    PurgeTrash,
    TrimVersions,
    CollectBlobs,
    ReconcileStorage,
    VerifyBlobs,
    FixUsage,
    PurgeAudit,
}

const STEPS: [Step; 8] = [
    Step::ExpireUploads,
    Step::PurgeTrash,
    Step::TrimVersions,
    Step::CollectBlobs,
    Step::ReconcileStorage,
    Step::VerifyBlobs,
    Step::FixUsage,
    Step::PurgeAudit,
];

impl Step {
    /// Steps that delete or move data (skipped when the clocks disagree).
    fn destructive(self) -> bool {
        matches!(
            self,
            Self::ExpireUploads
                | Self::PurgeTrash
                | Self::TrimVersions
                | Self::CollectBlobs
                | Self::ReconcileStorage
        )
    }

    /// Steps that read or change the object store.
    fn needs_store(self) -> bool {
        matches!(
            self,
            Self::CollectBlobs | Self::ReconcileStorage | Self::VerifyBlobs
        )
    }

    fn name(self) -> &'static str {
        match self {
            Self::ExpireUploads => "expire_uploads",
            Self::PurgeTrash => "purge_trash",
            Self::TrimVersions => "trim_versions",
            Self::CollectBlobs => "collect_blobs",
            Self::ReconcileStorage => "reconcile_storage",
            Self::VerifyBlobs => "verify_blobs",
            Self::FixUsage => "fix_usage",
            Self::PurgeAudit => "purge_audit",
        }
    }
}

// ---- GET /admin/maintenance ------------------------------------------------------

/// The newest finished run in its published form: times, duration, counts per
/// step and the error codes of failed steps. No tenant data.
pub(crate) fn run_json(record: &db::RunRecord) -> Value {
    let report: Value = serde_json::from_str(&record.report).unwrap_or(Value::Null);
    let steps = report.get("steps").cloned().unwrap_or_else(|| json!({}));
    let mut error_codes: Vec<String> = steps
        .as_object()
        .map(|steps| {
            steps
                .values()
                .filter_map(|step| step.get("error_code").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    error_codes.sort();
    error_codes.dedup();
    json!({
        "trigger": record.trigger,
        "started_at": rfc3339(record.started_at),
        "finished_at": rfc3339(record.finished_at),
        "duration_ms": record.duration_ms,
        "steps": steps,
        "error_codes": error_codes,
    })
}

/// Operator only: when the maintenance job last ran and what it did.
pub(crate) async fn get_status(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = operator_gate(
        &services,
        &session,
        Action::MaintenanceRead,
        "maintenance.read",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    match db::last_run(&services.pool).await {
        Ok(record) => {
            let last_run = record.as_ref().map(run_json);
            Json(json!({ "last_run": last_run })).into_response()
        }
        Err(error) => db_failure(&error, &headers, "maintenance status failed"),
    }
}
