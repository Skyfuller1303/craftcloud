//! `backup create`: a consistent snapshot of the database plus the blobs it
//! references.
//!
//! **Consistency.** All tables are exported with `COPY ... TO STDOUT` inside
//! one `REPEATABLE READ READ ONLY` transaction, so they show the same instant
//! (the recovery point, `created_at` of the manifest). Committed blobs are
//! immutable, so their bytes are copied afterwards.
//!
//! **No garbage collection meanwhile.** Before the snapshot the backup takes
//! the maintenance run lock (the PostgreSQL advisory lock that admits one
//! maintenance run at a time) and keeps it until the last blob is copied.
//! Retention purge, blob garbage collection and reconciliation only run under
//! that lock (in the server loop and in `admin maintenance --once`), so they
//! skip while a backup runs and nothing the snapshot references can disappear.
//! The lock belongs to the database connection: if the backup process dies or
//! loses its connection, PostgreSQL releases it, so a crashed backup cannot
//! block garbage collection for good (and a hung one is visible as an open
//! connection).
//!
//! **Uploads.** Only `committed` blobs are copied. An upload in progress is a
//! `staged` blob and an open upload session in the snapshot; the restore turns
//! those into aborted sessions (see `restore`).
//!
//! **Bounded memory.** COPY data, the blob list and the blob bytes are
//! streamed in blocks of at most 256 KiB; no table and no file is loaded
//! whole.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use craftcloud_db::PgPool;
use craftcloud_db::audit::{self, AuditCtx, AuditResult, Detail, NewEvent};
use craftcloud_db::maintenance as db;
use craftcloud_storage::{LocalStore, ObjectKey, ObjectStore, ReadSource, StorageError};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::archive::{ArchiveAccess, verify_remote_chunks, verify_remote_refs};
use crate::blob_stream::blob_entries;
use crate::error::{BackupError, ErrorKind};
use crate::manifest::{
    BLOB_INDEX_FILE, BLOBS_DIR, BlobEntry, BlobIndex, CHUNK_INDEX_FILE, CHUNKED_VERSION_INDEX_FILE,
    CHUNKS_DIR, ChunkIndex, ChunkedVersionEntry, FORMAT, FORMAT_VERSION, JsonlIndex, MANIFEST_FILE,
    MANIFEST_SUM_FILE, MAX_BLOB_LINE_BYTES, Manifest, StoreInfo, TABLES_DIR, TableEntry,
    VERSION_CHUNK_INDEX_FILE, VersionChunkEntry, blob_file, hex, parse_sha256, table_file,
};
use crate::tables::{backup_tables_for, excluded_tables_for};

/// Size of the write buffer in front of every output file.
const WRITE_BUFFER: usize = 256 * 1024;
/// Rows of the blob list read from the snapshot at once.
const BLOB_PAGE: i64 = 1000;
/// Archive pins are inserted in bounded batches inside the maintenance lock.
const ARCHIVE_PIN_BATCH: usize = 256;
type BlobRow = (
    Uuid,
    String,
    i64,
    Vec<u8>,
    String,
    Option<String>,
    Option<Vec<u8>>,
    String,
);
type ChunkRow = (
    Uuid,
    Uuid,
    String,
    i64,
    i64,
    String,
    Vec<u8>,
    String,
    Option<String>,
    Option<Vec<u8>>,
);

/// Points of a running backup at which other work can be interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The maintenance lock is held; nothing has been read yet.
    LockHeld,
    /// The snapshot is taken and closed; the blob bytes are not copied yet.
    SnapshotTaken,
    /// Every blob is copied; the manifest is not written yet.
    BlobsCopied,
}

/// A callback run at each [`Phase`], awaited before the backup continues.
/// Tests use it to run the maintenance job in exactly the gaps that the lock
/// must protect; the server passes none.
pub type PhaseHook = Arc<dyn Fn(Phase) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Parameters of [`create`].
#[derive(Clone)]
pub struct CreateOptions<'a> {
    /// Recorded in the manifest (`CARGO_PKG_VERSION` of the server binary).
    pub server_version: &'a str,
    /// How long to wait for a running maintenance job (or another backup)
    /// before giving up.
    pub lock_wait: Duration,
    /// Called at each [`Phase`]; `None` in production.
    pub on_phase: Option<PhaseHook>,
}

/// What a successful backup produced.
#[derive(Debug, Clone)]
pub struct CreateReport {
    /// The manifest as written.
    pub manifest: Manifest,
    /// SHA-256 of `manifest.json`; keep a copy off the backup medium.
    pub manifest_sha256: String,
    /// Stable id used to release archive retention only after all backup copies are deleted.
    pub backup_id: Uuid,
    /// Rows over all tables.
    pub rows: u64,
    /// Blob files copied.
    pub blobs: u64,
    /// Bytes of the blob files.
    pub blob_bytes: u64,
    /// Wall-clock time of the whole backup.
    pub duration: Duration,
    /// Whether the `backup.created` audit event was stored. When `false` the
    /// backup is complete and usable, but the health view does not know it.
    pub audit_recorded: bool,
}

/// An output file: written through a buffer, hashed and counted on the way.
pub(crate) struct Out {
    file: tokio::fs::File,
    hasher: Sha256,
    buffer: Vec<u8>,
    len: u64,
    newlines: u64,
    name: String,
}

/// The totals of a finished output file.
pub(crate) struct Written {
    pub len: u64,
    pub sha256: [u8; 32],
    pub newlines: u64,
}

impl Out {
    /// Creates a new file (never overwrites), mode 0600 on unix.
    pub(crate) fn create(path: &Path, name: &str) -> Result<Self, BackupError> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .map_err(|error| BackupError::io(&format!("cannot create {name}"), &error))?;
        Ok(Self {
            file: tokio::fs::File::from_std(file),
            hasher: Sha256::new(),
            buffer: Vec::with_capacity(WRITE_BUFFER),
            len: 0,
            newlines: 0,
            name: name.to_owned(),
        })
    }

    pub(crate) async fn write(&mut self, data: &[u8]) -> Result<(), BackupError> {
        self.hasher.update(data);
        self.len += data.len() as u64;
        self.newlines += data.iter().filter(|b| **b == b'\n').count() as u64;
        if self.buffer.len() + data.len() > WRITE_BUFFER {
            self.flush().await?;
        }
        if data.len() >= WRITE_BUFFER {
            self.file
                .write_all(data)
                .await
                .map_err(|error| BackupError::io(&format!("cannot write {}", self.name), &error))
        } else {
            self.buffer.extend_from_slice(data);
            Ok(())
        }
    }

    async fn flush(&mut self) -> Result<(), BackupError> {
        if !self.buffer.is_empty() {
            self.file
                .write_all(&self.buffer)
                .await
                .map_err(|error| BackupError::io(&format!("cannot write {}", self.name), &error))?;
            self.buffer.clear();
        }
        Ok(())
    }

    /// Flushes, forces the data to disk and returns the totals.
    pub(crate) async fn finish(mut self) -> Result<Written, BackupError> {
        self.flush().await?;
        self.file
            .sync_all()
            .await
            .map_err(|error| BackupError::io(&format!("cannot sync {}", self.name), &error))?;
        Ok(Written {
            len: self.len,
            sha256: self.hasher.finalize().into(),
            newlines: self.newlines,
        })
    }
}

/// Creates a directory that only the owner can use (0700 on unix).
fn make_dir(path: &Path, what: &str) -> Result<(), BackupError> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(BackupError::io(&format!("cannot create {what}"), &error)),
    }
}

/// Makes sure `out` is a new or empty directory; returns whether this call
/// created it.
fn prepare_output(out: &Path) -> Result<bool, BackupError> {
    match std::fs::symlink_metadata(out) {
        Ok(meta) if meta.is_dir() => {
            let mut entries = std::fs::read_dir(out)
                .map_err(|error| BackupError::io("cannot read the output directory", &error))?;
            if entries.next().is_some() {
                return Err(BackupError::new(
                    ErrorKind::Io,
                    "the output directory is not empty; a backup never writes into existing data",
                ));
            }
            // The directory was made by someone else: the backup holds every
            // password hash and file, so only the owner may use it (unix).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o700)).map_err(
                    |error| {
                        BackupError::io("cannot restrict the output directory to its owner", &error)
                    },
                )?;
            }
            Ok(false)
        }
        Ok(_) => Err(BackupError::new(
            ErrorKind::Io,
            "the output path exists and is not a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|error| {
                    BackupError::io("cannot create the parent directory", &error)
                })?;
            }
            make_dir(out, "the output directory")?;
            Ok(true)
        }
        Err(error) => Err(BackupError::io("cannot read the output path", &error)),
    }
}

/// Makes a consistent backup of the database behind `pool` and the objects of
/// `store` into the new or empty directory `out`.
///
/// # Errors
///
/// [`BackupError`]; on failure a directory created by this call is removed
/// again and a `backup.created` event with result `failed` is recorded on a
/// best-effort basis.
pub async fn create(
    pool: &PgPool,
    store: &LocalStore,
    out: &Path,
    options: &CreateOptions<'_>,
) -> Result<CreateReport, BackupError> {
    create_with_archive(pool, store, out, options, None).await
}

/// Creates a backup that may reference encrypted objects in an archive store.
/// The archive provider and crypto key are required only when the snapshot has
/// archive references.
pub async fn create_with_archive(
    pool: &PgPool,
    store: &LocalStore,
    out: &Path,
    options: &CreateOptions<'_>,
    archive: Option<ArchiveAccess<'_>>,
) -> Result<CreateReport, BackupError> {
    let started = Instant::now();
    let created_dir = prepare_output(out)?;
    let backup_id = Uuid::new_v4();
    match create_inner(pool, store, out, options, started, backup_id, archive).await {
        Ok(mut report) => {
            let event = NewEvent::new("backup.created")
                .detail(
                    "tables",
                    Detail::Count(count(report.manifest.tables.len() as u64)),
                )
                .detail("rows", Detail::Count(count(report.rows)))
                .detail("blobs", Detail::Count(count(report.blobs)))
                .target("archive_backup", report.backup_id)
                .detail("bytes", Detail::Count(count(report.blob_bytes)))
                .detail(
                    "migration",
                    Detail::Count(report.manifest.migration_version),
                );
            report.audit_recorded = audit::record(pool, &AuditCtx::system(), &event)
                .await
                .is_ok();
            if !report.audit_recorded {
                tracing::error!("the backup is complete but its audit event could not be stored");
            }
            Ok(report)
        }
        Err(error) => {
            let event = NewEvent::new("backup.created").result(AuditResult::Failed);
            if audit::record(pool, &AuditCtx::system(), &event)
                .await
                .is_err()
            {
                tracing::warn!("the failed backup could not be recorded in the audit log");
            }
            // Only what this call created; a partial backup is useless and
            // holds tenant data. An output directory that existed (empty) stays,
            // but the entries written into it go.
            if created_dir {
                let _ = std::fs::remove_dir_all(out);
            } else {
                remove_own_entries(out);
            }
            if let Err(cancel_error) = db::archive_backup_cancel(pool, backup_id).await {
                tracing::error!(%cancel_error, backup = %backup_id, "incomplete archive backup pins remain and require inventory recovery");
            }
            Err(error)
        }
    }
}

/// Removes what a backup writes at the top level of its output directory.
fn remove_own_entries(out: &Path) {
    for name in [TABLES_DIR, BLOBS_DIR, CHUNKS_DIR] {
        let _ = std::fs::remove_dir_all(out.join(name));
    }
    for name in [
        BLOB_INDEX_FILE,
        CHUNK_INDEX_FILE,
        CHUNKED_VERSION_INDEX_FILE,
        VERSION_CHUNK_INDEX_FILE,
        MANIFEST_FILE,
        MANIFEST_SUM_FILE,
    ] {
        let _ = std::fs::remove_file(out.join(name));
    }
}

fn count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

async fn create_inner(
    pool: &PgPool,
    store: &LocalStore,
    out: &Path,
    options: &CreateOptions<'_>,
    started: Instant,
    backup_id: Uuid,
    archive: Option<ArchiveAccess<'_>>,
) -> Result<CreateReport, BackupError> {
    craftcloud_db::migrate::verify(pool)
        .await
        .map_err(|error| {
            BackupError::new(
                ErrorKind::Schema,
                format!("{error}; run `craftcloud-server migrate` first"),
            )
        })?;

    // The store must be the one this database is bound to: otherwise the
    // backup would pair a database with foreign or missing objects.
    let binding = db::store_binding_epoch(pool).await?;
    let marker = store
        .read_marker_info()
        .await
        .map_err(|error| BackupError::new(ErrorKind::Binding, error.to_string()))?;
    let (store_id, epoch) = match (binding, marker) {
        (Some((id, epoch)), Some(info)) if id == info.store_id => (id, epoch),
        _ => {
            return Err(BackupError::new(
                ErrorKind::Binding,
                "the database and CRAFTCLOUD_STORAGE_DIR are not bound to each other \
                 (see `admin storage-bind`); refusing to back up an unpaired store",
            ));
        }
    };

    let lock = wait_for_lock(pool, options.lock_wait).await?;
    let result = locked(LockedCreate {
        pool,
        store,
        out,
        options,
        started,
        store_id,
        epoch,
        backup_id,
        archive,
    })
    .await;
    if let Err(error) = lock.release().await {
        tracing::warn!(%error, "the maintenance lock could not be released explicitly; the connection is closed instead");
    }
    result
}

async fn wait_for_lock(
    pool: &PgPool,
    wait: Duration,
) -> Result<craftcloud_db::maintenance::RunLock, BackupError> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(lock) = db::try_lock(pool).await? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            return Err(BackupError::new(
                ErrorKind::Busy,
                "the maintenance job (or another backup) is running; try again when it has finished",
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

struct LockedCreate<'a, 'options, 'archive> {
    pool: &'a PgPool,
    store: &'a LocalStore,
    out: &'a Path,
    options: &'options CreateOptions<'archive>,
    started: Instant,
    store_id: Uuid,
    epoch: i64,
    backup_id: Uuid,
    archive: Option<ArchiveAccess<'archive>>,
}

async fn locked(context: LockedCreate<'_, '_, '_>) -> Result<CreateReport, BackupError> {
    let LockedCreate {
        pool,
        store,
        out,
        options,
        started,
        store_id,
        epoch,
        backup_id,
        archive,
    } = context;
    notify(options, Phase::LockHeld).await;
    make_dir(&out.join(TABLES_DIR), "the tables directory")?;
    make_dir(&out.join(BLOBS_DIR), "the blobs directory")?;

    let snapshot = snapshot(pool, out, store, archive).await?;
    notify(options, Phase::SnapshotTaken).await;
    let (blobs, blob_bytes) = copy_blobs(store, out).await?;
    let (chunks, chunk_bytes) = copy_chunks(store, out).await?;
    if chunks != snapshot.local_chunk_count || chunk_bytes != snapshot.local_chunk_bytes {
        return Err(BackupError::new(
            ErrorKind::Integrity,
            "copied chunks do not match the chunk list",
        ));
    }
    notify(options, Phase::BlobsCopied).await;
    if blobs != snapshot.local_blob_count || blob_bytes != snapshot.local_blob_bytes {
        return Err(BackupError::new(
            ErrorKind::Integrity,
            "the copied blobs do not match the blob list",
        ));
    }

    let references = snapshot.archive_ref_count;
    if references > 0 {
        let access = archive.ok_or_else(|| {
            BackupError::new(
                ErrorKind::SourceDamaged,
                "archive-aware backup requires the configured archive provider and server key",
            )
        })?;
        let sum = verify_remote_refs(access, backup_id, out).await?;
        if Some(sum.sha256) != parse_sha256(&snapshot.index.sha256)
            || sum.count != snapshot.index.count
            || sum.total_bytes != snapshot.index.total_bytes
        {
            return Err(BackupError::new(
                ErrorKind::Integrity,
                "the archive verification did not match the blob list",
            ));
        }
        verify_remote_chunks(access, backup_id, out).await?;
    }

    // Pins become durable before the manifest is published. A crash here can
    // leak retention, but cannot leave a completed backup without its archive keys.
    // A backup without archive references needs no pins and leaves no rows behind.
    let pinned_refs = if references > 0 {
        register_archive_pins(
            pool,
            out,
            backup_id,
            &snapshot.index,
            Some(&snapshot.chunks),
        )
        .await?
    } else {
        0
    };

    let rows = snapshot.tables.iter().map(|t| t.rows).sum();
    let manifest = Manifest {
        format: FORMAT.to_owned(),
        format_version: FORMAT_VERSION,
        created_at: snapshot.taken_at,
        server_version: options.server_version.to_owned(),
        migration_version: snapshot.migration_version,
        store: StoreInfo {
            store_id: store_id.to_string(),
            epoch,
        },
        tables: snapshot.tables,
        blobs: snapshot.index,
        chunks: Some(snapshot.chunks),
        chunked_versions: Some(snapshot.chunked_versions),
        version_chunks: Some(snapshot.version_chunks),
        excluded_tables: excluded_tables_for(snapshot.migration_version)
            .iter()
            .map(|t| (*t).to_owned())
            .collect(),
        unavailable_version_blobs: snapshot.unavailable,
        backup_id: Some(backup_id.to_string()),
    };
    if manifest.unavailable_version_blobs > 0 {
        tracing::warn!(
            versions = manifest.unavailable_version_blobs,
            "some file versions reference blobs that were not committed (for example corrupt); their bytes are not in this backup"
        );
    }
    let bytes = manifest.to_bytes()?;
    let digest = hex(&Sha256::digest(&bytes));
    write_small(out, MANIFEST_FILE, &bytes).await?;
    write_small(
        out,
        MANIFEST_SUM_FILE,
        format!("{digest}  {MANIFEST_FILE}\n").as_bytes(),
    )
    .await?;
    sync_dir(out).await;
    if references > 0 && !db::archive_backup_complete(pool, backup_id, &digest, pinned_refs).await?
    {
        return Err(BackupError::new(
            ErrorKind::Integrity,
            "the archive backup pins do not match the manifest",
        ));
    }
    Ok(CreateReport {
        manifest,
        manifest_sha256: digest,
        backup_id,
        rows,
        blobs,
        blob_bytes,
        duration: started.elapsed(),
        audit_recorded: false,
    })
}

pub(crate) async fn register_archive_pins(
    pool: &PgPool,
    dir: &Path,
    backup_id: Uuid,
    expected: &BlobIndex,
    expected_chunks: Option<&ChunkIndex>,
) -> Result<u64, BackupError> {
    db::archive_backup_begin(pool, backup_id).await?;
    let mut entries = blob_entries(dir);
    let mut batch = Vec::with_capacity(ARCHIVE_PIN_BATCH);
    let mut count = 0_u64;
    while let Some(entry) = entries.next().await? {
        if let Some(key) = entry.archive_object_key {
            let blob_id = Uuid::parse_str(&entry.id)
                .map_err(|_| BackupError::new(ErrorKind::Format, "a blob id is invalid"))?;
            batch.push(db::ArchiveBackupRef {
                blob_id,
                archive_object_key: key,
            });
            count = count.checked_add(1).ok_or_else(|| {
                BackupError::new(ErrorKind::Integrity, "archive reference count overflow")
            })?;
            if batch.len() == ARCHIVE_PIN_BATCH {
                db::archive_backup_add_refs(pool, backup_id, &batch).await?;
                batch.clear();
            }
        }
    }
    if !batch.is_empty() {
        db::archive_backup_add_refs(pool, backup_id, &batch).await?;
    }
    let sum = entries
        .summary()
        .ok_or_else(|| BackupError::new(ErrorKind::Io, "the blob list was not fully read"))?;
    if sum.count != expected.count
        || sum.total_bytes != expected.total_bytes
        || parse_sha256(&expected.sha256) != Some(sum.sha256)
    {
        return Err(BackupError::new(
            ErrorKind::Integrity,
            "blobs.jsonl changed while archive pins were registered",
        ));
    }
    if let Some(expected_chunks) = expected_chunks {
        let mut file = tokio::fs::File::open(dir.join(CHUNK_INDEX_FILE))
            .await
            .map_err(|error| BackupError::io("cannot read the chunk list", &error))?;
        let mut reader = tokio::io::BufReader::new(&mut file);
        let mut line = Vec::new();
        let mut hasher = Sha256::new();
        let mut entries = 0_u64;
        let mut total = 0_u64;
        let mut chunk_batch = Vec::with_capacity(ARCHIVE_PIN_BATCH);
        loop {
            line.clear();
            let got = (&mut reader)
                .take(MAX_BLOB_LINE_BYTES as u64 + 2)
                .read_until(b'\n', &mut line)
                .await
                .map_err(|error| BackupError::io("cannot read the chunk list", &error))?;
            if got == 0 {
                break;
            }
            hasher.update(&line);
            if line.pop() != Some(b'\n') {
                return Err(BackupError::new(
                    ErrorKind::Format,
                    "the chunk list is damaged",
                ));
            }
            let entry = crate::manifest::ChunkEntry::parse_line(&line)?;
            entries += 1;
            total = total.checked_add(entry.stored_len()).ok_or_else(|| {
                BackupError::new(ErrorKind::Integrity, "chunk size total overflow")
            })?;
            if let Some(key) = entry.archive_object_key {
                chunk_batch.push(db::ArchiveChunkBackupRef {
                    chunk_id: Uuid::parse_str(&entry.id).map_err(|_| {
                        BackupError::new(ErrorKind::Format, "a chunk id is invalid")
                    })?,
                    archive_object_key: key,
                });
                count = count.checked_add(1).ok_or_else(|| {
                    BackupError::new(ErrorKind::Integrity, "archive reference count overflow")
                })?;
                if chunk_batch.len() == ARCHIVE_PIN_BATCH {
                    db::archive_backup_add_chunk_refs(pool, backup_id, &chunk_batch).await?;
                    chunk_batch.clear();
                }
            }
        }
        if !chunk_batch.is_empty() {
            db::archive_backup_add_chunk_refs(pool, backup_id, &chunk_batch).await?;
        }
        if entries != expected_chunks.count
            || total != expected_chunks.total_bytes
            || hex(&hasher.finalize()) != expected_chunks.sha256
        {
            return Err(BackupError::new(
                ErrorKind::Integrity,
                "chunks.jsonl changed while archive pins were registered",
            ));
        }
    }
    Ok(count)
}

async fn notify(options: &CreateOptions<'_>, phase: Phase) {
    if let Some(hook) = &options.on_phase {
        hook(phase).await;
    }
}

async fn write_small(out: &Path, name: &str, data: &[u8]) -> Result<(), BackupError> {
    let mut file = Out::create(&out.join(name), name)?;
    file.write(data).await?;
    file.finish().await.map(|_| ())
}

#[cfg(unix)]
async fn sync_dir(path: &Path) {
    if let Ok(dir) = tokio::fs::File::open(path).await {
        let _ = dir.sync_all().await;
    }
}

#[cfg(not(unix))]
async fn sync_dir(_path: &Path) {}

struct Snapshot {
    taken_at: String,
    migration_version: i64,
    tables: Vec<TableEntry>,
    index: BlobIndex,
    chunks: ChunkIndex,
    chunked_versions: JsonlIndex,
    version_chunks: JsonlIndex,
    local_blob_count: u64,
    local_blob_bytes: u64,
    archive_ref_count: u64,
    local_chunk_count: u64,
    local_chunk_bytes: u64,
    unavailable: u64,
}

/// Exports every table and the list of committed blobs from one snapshot.
async fn snapshot(
    pool: &PgPool,
    out: &Path,
    store: &LocalStore,
    archive: Option<ArchiveAccess<'_>>,
) -> Result<Snapshot, BackupError> {
    let mut tx = pool.begin().await?;
    // First statement of the transaction: all later reads see one instant.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let migration_version: i64 =
        sqlx::query_scalar("SELECT COALESCE(max(version), 0) FROM _sqlx_migrations WHERE success")
            .fetch_one(&mut *tx)
            .await?;
    let taken_at: time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *tx)
        .await?;
    let taken_at = taken_at
        .to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| {
            BackupError::new(ErrorKind::Database, "the snapshot time cannot be formatted")
        })?;

    let specs = backup_tables_for(migration_version)?;
    let mut tables = Vec::with_capacity(specs.len());
    for spec in specs {
        let relative = table_file(spec.name);
        let mut file = Out::create(&out.join(&relative), &relative)?;
        let copy = match spec.name {
            // Remote identities and snapshot attempts belong to the old
            // FigCraft server. A restored file must be republished, with no
            // prior remote id or automatic-snapshot completion state.
            "figcraft_live_files" => "COPY (SELECT file_id,org_id,workspace_id,NULL::text,published_from_version_id,last_snapshot_version_id,last_snapshot_at,NULL::timestamptz,false,'republish',published_by_user_id,created_at,now() FROM figcraft_live_files) TO STDOUT".to_owned(),
            // From schema 20 the link also carries its origin (source_kind,
            // source_app); the restore loads without a column list.
            "figcraft_links" if migration_version >= 20 => "COPY (SELECT id,file_id,user_id,access,NULL::text,NULL::bytea,created_at,COALESCE(revoked_at,now()),source_kind,source_app FROM figcraft_links) TO STDOUT".to_owned(),
            "figcraft_links" => "COPY (SELECT id,file_id,user_id,access,NULL::text,NULL::bytea,created_at,COALESCE(revoked_at,now()) FROM figcraft_links) TO STDOUT".to_owned(),
            // Remote revocation jobs cannot target the restored server's new
            // FigCraft file ids. Restored personal links are already revoked.
            "figcraft_outbox" => "COPY (SELECT id,file_id,user_id,reason,attempts,available_at,NULL::timestamptz,created_at FROM figcraft_outbox WHERE false) TO STDOUT".to_owned(),
            _ => format!("COPY \"{}\" TO STDOUT", spec.name),
        };
        let mut stream = tx.copy_out_raw(&copy).await?;
        while let Some(chunk) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            file.write(&chunk?).await?;
        }
        drop(stream);
        let written = file.finish().await?;
        // Newlines inside values are escaped by COPY: one line, one row.
        tables.push(TableEntry {
            name: spec.name.to_owned(),
            file: relative,
            rows: written.newlines,
            bytes: written.len,
            sha256: hex(&written.sha256),
        });
    }

    let unavailable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM file_versions v JOIN blobs b ON b.org_id = v.org_id AND b.id = v.blob_id \
         WHERE b.state <> 'committed'",
    )
    .fetch_one(&mut *tx)
    .await?;

    let mut chunk_list = Out::create(&out.join(CHUNK_INDEX_FILE), CHUNK_INDEX_FILE)?;
    let mut chunk_count = 0_u64;
    let mut chunk_total = 0_u64;
    let mut chunk_archive_refs = 0_u64;
    let mut local_chunk_count = 0_u64;
    let mut local_chunk_bytes = 0_u64;
    let mut chunk_last = String::new();
    if migration_version >= 25 {
        loop {
            let rows: Vec<ChunkRow> = sqlx::query_as(
            if migration_version >= 27 { "SELECT c.id,c.file_id,c.object_key,c.size_bytes,c.stored_bytes,c.encoding,c.sha256,c.storage_tier,c.archive_object_key,c.archive_wrapped_key \
             FROM chunks c WHERE EXISTS (SELECT 1 FROM version_chunks vc WHERE vc.chunk_id=c.id) AND c.id::text > $1 \
             ORDER BY c.id::text LIMIT $2" } else { "SELECT c.id,c.file_id,c.object_key,c.size_bytes,c.size_bytes,'raw'::text,c.sha256,c.storage_tier,c.archive_object_key,c.archive_wrapped_key \
             FROM chunks c WHERE EXISTS (SELECT 1 FROM version_chunks vc WHERE vc.chunk_id=c.id) AND c.id::text > $1 \
             ORDER BY c.id::text LIMIT $2" },
        ).bind(&chunk_last).bind(BLOB_PAGE).fetch_all(&mut *tx).await?;
            let Some((last_id, _, _, _, _, _, _, _, _, _)) = rows.last() else {
                break;
            };
            chunk_last = last_id.to_string();
            for (
                id,
                file_id,
                key,
                size,
                stored_bytes,
                encoding,
                sha256,
                tier,
                archive_key,
                wrapped,
            ) in rows
            {
                let size = u64::try_from(size).map_err(|_| {
                    BackupError::new(ErrorKind::SourceDamaged, "a chunk has a negative size")
                })?;
                let local_copy = match tier.as_str() {
                    "local" | "archiving" => true,
                    "archive" => false,
                    _ => {
                        return Err(BackupError::new(
                            ErrorKind::SourceDamaged,
                            "a chunk has an unknown storage tier",
                        ));
                    }
                };
                let (archive_object_key, archive_wrapped_key) = if local_copy {
                    (None, None)
                } else {
                    match (archive_key, wrapped) {
                        (Some(k), Some(w)) => (Some(k), Some(hex(&w))),
                        _ => {
                            return Err(BackupError::new(
                                ErrorKind::SourceDamaged,
                                "archived chunk metadata is incomplete",
                            ));
                        }
                    }
                };
                let entry = crate::manifest::ChunkEntry {
                    id: id.to_string(),
                    file_id: file_id.to_string(),
                    key,
                    size,
                    stored_bytes: u64::try_from(stored_bytes).map_err(|_| {
                        BackupError::new(
                            ErrorKind::SourceDamaged,
                            "a chunk has an invalid stored size",
                        )
                    })?,
                    encoding,
                    sha256: hex(&sha256),
                    local_copy,
                    archive_object_key,
                    archive_wrapped_key,
                };
                if entry.archive_object_key.is_some() {
                    chunk_archive_refs = chunk_archive_refs.checked_add(1).ok_or_else(|| {
                        BackupError::new(
                            ErrorKind::Integrity,
                            "chunk archive reference count overflow",
                        )
                    })?;
                }
                if local_copy {
                    local_chunk_count = local_chunk_count.checked_add(1).ok_or_else(|| {
                        BackupError::new(ErrorKind::Integrity, "local chunk count overflow")
                    })?;
                    local_chunk_bytes = local_chunk_bytes
                        .checked_add(entry.stored_len())
                        .ok_or_else(|| {
                            BackupError::new(
                                ErrorKind::Integrity,
                                "local chunk byte count overflow",
                            )
                        })?;
                }
                let mut line = serde_json::to_vec(&entry).map_err(|_| {
                    BackupError::new(ErrorKind::Format, "a chunk entry cannot be written")
                })?;
                crate::manifest::ChunkEntry::parse_line(&line).map_err(|_| {
                    BackupError::new(ErrorKind::SourceDamaged, "a chunk row has invalid metadata")
                })?;
                line.push(b'\n');
                chunk_list.write(&line).await?;
                chunk_count = chunk_count.checked_add(1).ok_or_else(|| {
                    BackupError::new(ErrorKind::Integrity, "chunk count overflow")
                })?;
                chunk_total = chunk_total.checked_add(entry.stored_len()).ok_or_else(|| {
                    BackupError::new(ErrorKind::Integrity, "chunk sizes overflow")
                })?;
            }
        }
    }
    let chunk_written = chunk_list.finish().await?;

    let mut versions_out = Out::create(
        &out.join(CHUNKED_VERSION_INDEX_FILE),
        CHUNKED_VERSION_INDEX_FILE,
    )?;
    let mut versions_count = 0_u64;
    let mut versions_size = 0_u64;
    let mut version_cursor = String::new();
    if migration_version >= 25 {
        loop {
            let versions: Vec<(Uuid, i64, Vec<u8>)> = sqlx::query_as(
                "SELECT v.id,v.size_bytes,v.sha256 FROM file_versions v JOIN blobs b ON b.org_id=v.org_id AND b.id=v.blob_id WHERE b.layout='chunked' AND b.state='committed' AND v.id::text > $1 ORDER BY v.id::text LIMIT $2",
            ).bind(&version_cursor).bind(BLOB_PAGE).fetch_all(&mut *tx).await?;
            let Some((last, _, _)) = versions.last() else {
                break;
            };
            version_cursor = last.to_string();
            for (id, size, sha256) in versions {
                let entry = ChunkedVersionEntry {
                    id: id.to_string(),
                    size: u64::try_from(size).map_err(|_| {
                        BackupError::new(
                            ErrorKind::SourceDamaged,
                            "a chunked version has a negative size",
                        )
                    })?,
                    sha256: hex(&sha256),
                };
                let mut line = serde_json::to_vec(&entry).map_err(|_| {
                    BackupError::new(
                        ErrorKind::Format,
                        "a chunked version entry cannot be written",
                    )
                })?;
                ChunkedVersionEntry::parse_line(&line)?;
                line.push(b'\n');
                versions_out.write(&line).await?;
                versions_count += 1;
                versions_size = versions_size.checked_add(entry.size).ok_or_else(|| {
                    BackupError::new(ErrorKind::Integrity, "chunked version sizes overflow")
                })?;
            }
        }
    }
    let versions_written = versions_out.finish().await?;
    let mut mappings_out = Out::create(
        &out.join(VERSION_CHUNK_INDEX_FILE),
        VERSION_CHUNK_INDEX_FILE,
    )?;
    let mut mappings_count = 0_u64;
    let mut mappings_size = 0_u64;
    let mut mapping_cursor = String::new();
    let mut mapping_ordinal = -1_i32;
    if migration_version >= 25 {
        loop {
            type MappingRow = (
                Uuid,
                i32,
                i64,
                Uuid,
                Uuid,
                String,
                i64,
                i64,
                String,
                Vec<u8>,
                String,
                Option<String>,
                Option<Vec<u8>>,
            );
            let rows: Vec<MappingRow> = sqlx::query_as(
                if migration_version >= 27 { "SELECT vc.version_id,vc.ordinal,vc.byte_offset,c.id,vc.file_id,c.object_key,c.size_bytes,c.stored_bytes,c.encoding,c.sha256,c.storage_tier,c.archive_object_key,c.archive_wrapped_key FROM version_chunks vc JOIN chunks c ON c.id=vc.chunk_id AND c.file_id=vc.file_id JOIN file_versions v ON v.id=vc.version_id JOIN blobs b ON b.org_id=v.org_id AND b.id=v.blob_id WHERE b.layout='chunked' AND b.state='committed' AND (vc.version_id::text,vc.ordinal) > ($1, $2) ORDER BY vc.version_id::text,vc.ordinal LIMIT $3" } else { "SELECT vc.version_id,vc.ordinal,vc.byte_offset,c.id,vc.file_id,c.object_key,c.size_bytes,c.size_bytes,'raw'::text,c.sha256,c.storage_tier,c.archive_object_key,c.archive_wrapped_key FROM version_chunks vc JOIN chunks c ON c.id=vc.chunk_id AND c.file_id=vc.file_id JOIN file_versions v ON v.id=vc.version_id JOIN blobs b ON b.org_id=v.org_id AND b.id=v.blob_id WHERE b.layout='chunked' AND b.state='committed' AND (vc.version_id::text,vc.ordinal) > ($1, $2) ORDER BY vc.version_id::text,vc.ordinal LIMIT $3" },
            ).bind(&mapping_cursor).bind(mapping_ordinal).bind(BLOB_PAGE).fetch_all(&mut *tx).await?;
            let Some((last, last_ordinal, _, _, _, _, _, _, _, _, _, _, _)) = rows.last() else {
                break;
            };
            mapping_cursor = last.to_string();
            mapping_ordinal = *last_ordinal;
            for (
                version_id,
                ordinal,
                byte_offset,
                id,
                file_id,
                key,
                size,
                stored_bytes,
                encoding,
                sha256,
                tier,
                archive_key,
                wrapped,
            ) in rows
            {
                let local_copy = matches!(tier.as_str(), "local" | "archiving");
                let (archive_object_key, archive_wrapped_key) = if local_copy {
                    (None, None)
                } else {
                    (archive_key, wrapped.map(|value| hex(&value)))
                };
                let entry = VersionChunkEntry {
                    version_id: version_id.to_string(),
                    ordinal: u32::try_from(ordinal).map_err(|_| {
                        BackupError::new(ErrorKind::SourceDamaged, "a chunk ordinal is invalid")
                    })?,
                    byte_offset: u64::try_from(byte_offset).map_err(|_| {
                        BackupError::new(ErrorKind::SourceDamaged, "a chunk offset is invalid")
                    })?,
                    chunk: crate::manifest::ChunkEntry {
                        id: id.to_string(),
                        file_id: file_id.to_string(),
                        key,
                        size: u64::try_from(size).map_err(|_| {
                            BackupError::new(ErrorKind::SourceDamaged, "a chunk size is invalid")
                        })?,
                        stored_bytes: u64::try_from(stored_bytes).map_err(|_| {
                            BackupError::new(
                                ErrorKind::SourceDamaged,
                                "a chunk stored size is invalid",
                            )
                        })?,
                        encoding,
                        sha256: hex(&sha256),
                        local_copy,
                        archive_object_key,
                        archive_wrapped_key,
                    },
                };
                let mut line = serde_json::to_vec(&entry).map_err(|_| {
                    BackupError::new(ErrorKind::Format, "a version chunk entry cannot be written")
                })?;
                VersionChunkEntry::parse_line(&line)?;
                line.push(b'\n');
                mappings_out.write(&line).await?;
                mappings_count += 1;
                mappings_size = mappings_size.checked_add(entry.chunk.size).ok_or_else(|| {
                    BackupError::new(ErrorKind::Integrity, "version chunk sizes overflow")
                })?;
            }
        }
    }
    let mappings_written = mappings_out.finish().await?;

    let mut list = Out::create(&out.join(BLOB_INDEX_FILE), BLOB_INDEX_FILE)?;
    let (
        mut blob_count,
        mut blob_total,
        mut local_blob_count,
        mut local_blob_bytes,
        mut archive_ref_count,
    ) = (0_u64, 0_u64, 0_u64, 0_u64, 0_u64);
    let mut last = String::new();
    let mut previous: Option<String> = None;
    loop {
        let query = if migration_version >= 25 {
            "SELECT id, object_key, size_bytes, sha256, tier_state, archive_object_key, archive_wrapped_key, layout FROM blobs WHERE state = 'committed' AND object_key COLLATE \"C\" > $1 ORDER BY object_key COLLATE \"C\" LIMIT $2"
        } else if migration_version >= 22 {
            "SELECT id, object_key, size_bytes, sha256, tier_state, archive_object_key, archive_wrapped_key, 'whole'::text FROM blobs WHERE state = 'committed' AND object_key COLLATE \"C\" > $1 ORDER BY object_key COLLATE \"C\" LIMIT $2"
        } else {
            "SELECT id, object_key, size_bytes, sha256, 'local'::text, NULL::text, NULL::bytea, 'whole'::text FROM blobs WHERE state = 'committed' AND object_key COLLATE \"C\" > $1 ORDER BY object_key COLLATE \"C\" LIMIT $2"
        };
        let page: Vec<BlobRow> = sqlx::query_as(query)
            .bind(&last)
            .bind(BLOB_PAGE)
            .fetch_all(&mut *tx)
            .await?;
        let Some((_, newest, _, _, _, _, _, _)) = page.last() else {
            break;
        };
        last.clone_from(newest);
        for (id, key, size, sha256, tier_state, archive_object_key, archive_wrapped_key, layout) in
            &page
        {
            let size = u64::try_from(*size).map_err(|_| {
                BackupError::new(ErrorKind::SourceDamaged, "a blob has a negative size")
            })?;
            let local_copy = match tier_state.as_str() {
                "local" => true,
                "archived" => false,
                "archiving" | "recalling" => {
                    return Err(BackupError::new(
                        ErrorKind::Busy,
                        "an archive transition is incomplete; finish maintenance and retry the backup",
                    ));
                }
                _ => {
                    return Err(BackupError::new(
                        ErrorKind::SourceDamaged,
                        "a blob has an unknown storage tier",
                    ));
                }
            };
            // A local row may hold a durable archive upload intent whose
            // provider object is not yet committed. Keep its database columns
            // in the table snapshot, but never treat the pending key as a
            // verified remote backup reference.
            let (archive_object_key, archive_wrapped_key) = if local_copy {
                (None, None)
            } else {
                match (archive_object_key, archive_wrapped_key) {
                    (Some(key), Some(wrapped)) => (Some(key.clone()), Some(hex(wrapped))),
                    _ => {
                        return Err(BackupError::new(
                            ErrorKind::SourceDamaged,
                            "archived blob metadata is incomplete",
                        ));
                    }
                }
            };
            let entry = BlobEntry {
                id: id.to_string(),
                key: key.clone(),
                size,
                layout: layout.clone(),
                sha256: hex(sha256),
                local_copy: local_copy && layout == "whole",
                archive_object_key,
                archive_wrapped_key,
            };
            let mut line = serde_json::to_vec(&entry).map_err(|_| {
                BackupError::new(ErrorKind::Format, "a blob entry cannot be written")
            })?;
            // Rejects a database row that the restore would refuse to read.
            BlobEntry::parse_line(&line).map_err(|_| {
                BackupError::new(
                    ErrorKind::SourceDamaged,
                    "a blob row has an invalid key or checksum",
                )
            })?;
            line.push(b'\n');
            if previous.as_deref().is_some_and(|p| p >= key.as_str()) {
                return Err(BackupError::new(
                    ErrorKind::Integrity,
                    "the blob keys did not come out in byte order",
                ));
            }
            previous = Some(key.clone());
            list.write(&line).await?;
            blob_count += 1;
            blob_total = blob_total
                .checked_add(size)
                .ok_or_else(|| BackupError::new(ErrorKind::Integrity, "the blob sizes overflow"))?;
            if local_copy && layout == "whole" {
                local_blob_count = local_blob_count.checked_add(1).ok_or_else(|| {
                    BackupError::new(ErrorKind::Integrity, "the local blob count overflow")
                })?;
                local_blob_bytes = local_blob_bytes.checked_add(size).ok_or_else(|| {
                    BackupError::new(ErrorKind::Integrity, "the local blob sizes overflow")
                })?;
            }
            if entry.archive_object_key.is_some() {
                archive_ref_count = archive_ref_count.checked_add(1).ok_or_else(|| {
                    BackupError::new(ErrorKind::Integrity, "the archive reference count overflow")
                })?;
            }
        }
    }
    archive_ref_count = archive_ref_count
        .checked_add(chunk_archive_refs)
        .ok_or_else(|| {
            BackupError::new(ErrorKind::Integrity, "archive reference count overflow")
        })?;
    let written = list.finish().await?;
    if migration_version >= 25 {
        verify_chunked_versions(&mut tx, migration_version, store, archive).await?;
    }
    // The snapshot has served its purpose; do not hold it while copying files.
    tx.commit().await?;
    Ok(Snapshot {
        taken_at,
        migration_version,
        tables,
        index: BlobIndex {
            file: BLOB_INDEX_FILE.to_owned(),
            count: blob_count,
            total_bytes: blob_total,
            sha256: hex(&written.sha256),
        },
        chunks: ChunkIndex {
            file: CHUNK_INDEX_FILE.to_owned(),
            count: chunk_count,
            total_bytes: chunk_total,
            sha256: hex(&chunk_written.sha256),
        },
        chunked_versions: JsonlIndex {
            file: CHUNKED_VERSION_INDEX_FILE.to_owned(),
            count: versions_count,
            total_bytes: versions_size,
            sha256: hex(&versions_written.sha256),
        },
        version_chunks: JsonlIndex {
            file: VERSION_CHUNK_INDEX_FILE.to_owned(),
            count: mappings_count,
            total_bytes: mappings_size,
            sha256: hex(&mappings_written.sha256),
        },
        local_blob_count,
        local_blob_bytes,
        archive_ref_count,
        local_chunk_count,
        local_chunk_bytes,
        unavailable: u64::try_from(unavailable).unwrap_or(0),
    })
}

async fn verify_chunked_versions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    migration_version: i64,
    store: &LocalStore,
    archive: Option<ArchiveAccess<'_>>,
) -> Result<(), BackupError> {
    let mut cursor = String::new();
    loop {
        let versions: Vec<(Uuid, i64, Vec<u8>)> = sqlx::query_as(
            "SELECT v.id,v.size_bytes,v.sha256 FROM file_versions v JOIN blobs b ON b.org_id=v.org_id AND b.id=v.blob_id \
             WHERE b.layout='chunked' AND b.state='committed' AND v.id::text > $1 ORDER BY v.id::text LIMIT $2",
        ).bind(&cursor).bind(BLOB_PAGE).fetch_all(&mut **tx).await?;
        let Some((last, _, _)) = versions.last() else {
            break;
        };
        cursor = last.to_string();
        for (version_id, expected_size, expected_sha) in versions {
            let expected_size = u64::try_from(expected_size).map_err(|_| {
                BackupError::new(
                    ErrorKind::SourceDamaged,
                    "a chunked version has a negative size",
                )
            })?;
            let mut digest = Sha256::new();
            let mut size = 0_u64;
            let mut next_ordinal = 0_i32;
            let mut offset = 0_i64;
            loop {
                type ChunkRow = (
                    i32,
                    i64,
                    Uuid,
                    Uuid,
                    String,
                    i64,
                    i64,
                    String,
                    Vec<u8>,
                    String,
                    Option<String>,
                    Option<Vec<u8>>,
                );
                let rows:Vec<ChunkRow>=sqlx::query_as(
                    if migration_version >= 27 { "SELECT vc.ordinal,vc.byte_offset,c.id,vc.file_id,c.object_key,c.size_bytes,c.stored_bytes,c.encoding,c.sha256,c.storage_tier,c.archive_object_key,c.archive_wrapped_key \
                     FROM version_chunks vc JOIN chunks c ON c.id=vc.chunk_id AND c.file_id=vc.file_id \
                     WHERE vc.version_id=$1 AND vc.ordinal >= $2 ORDER BY vc.ordinal LIMIT $3" } else { "SELECT vc.ordinal,vc.byte_offset,c.id,vc.file_id,c.object_key,c.size_bytes,c.size_bytes,'raw'::text,c.sha256,c.storage_tier,c.archive_object_key,c.archive_wrapped_key \
                     FROM version_chunks vc JOIN chunks c ON c.id=vc.chunk_id AND c.file_id=vc.file_id \
                     WHERE vc.version_id=$1 AND vc.ordinal >= $2 ORDER BY vc.ordinal LIMIT $3" },
                ).bind(version_id).bind(next_ordinal).bind(BLOB_PAGE).fetch_all(&mut **tx).await?;
                if rows.is_empty() {
                    break;
                }
                for (
                    ordinal,
                    byte_offset,
                    chunk_id,
                    file_id,
                    key,
                    chunk_size,
                    stored_size,
                    encoding,
                    chunk_sha,
                    tier,
                    archive_key,
                    wrapped,
                ) in rows
                {
                    if ordinal != next_ordinal || byte_offset != offset {
                        return Err(BackupError::new(
                            ErrorKind::SourceDamaged,
                            format!("version {version_id} has a discontinuous chunk manifest"),
                        ));
                    }
                    let chunk_size = u64::try_from(chunk_size).map_err(|_| {
                        BackupError::new(ErrorKind::SourceDamaged, "a chunk has a negative size")
                    })?;
                    let stored_size = u64::try_from(stored_size).map_err(|_| {
                        BackupError::new(
                            ErrorKind::SourceDamaged,
                            "a chunk has an invalid stored size",
                        )
                    })?;
                    if chunk_size == 0
                        || chunk_size > craftcloud_storage::MAX_CHUNK_BYTES as u64
                        || stored_size == 0
                        || stored_size > craftcloud_storage::MAX_CHUNK_BYTES as u64
                    {
                        return Err(BackupError::new(
                            ErrorKind::SourceDamaged,
                            "a chunk exceeds the supported stored or plaintext size",
                        ));
                    }
                    let mut chunk_digest = Sha256::new();
                    let chunk_actual;
                    match tier.as_str() {
                        "local" | "archiving" => {
                            let key = ObjectKey::parse(&key).map_err(|_| {
                                BackupError::new(
                                    ErrorKind::SourceDamaged,
                                    "a chunk object key is invalid",
                                )
                            })?;
                            let mut source =
                                store.open_read(&key, None).await.map_err(|error| {
                                    if matches!(error, StorageError::NotFound) {
                                        BackupError::new(
                                            ErrorKind::SourceDamaged,
                                            format!("chunk {chunk_id} is missing from the store"),
                                        )
                                    } else {
                                        error.into()
                                    }
                                })?;
                            if source.remaining() != stored_size {
                                return Err(BackupError::new(
                                    ErrorKind::SourceDamaged,
                                    format!("chunk {chunk_id} has an unexpected size"),
                                ));
                            }
                            let mut stored =
                                Vec::with_capacity(usize::try_from(stored_size).unwrap_or(0));
                            while let Some(block) = source.next_chunk().await? {
                                stored.extend_from_slice(&block);
                            }
                            let entry = crate::manifest::ChunkEntry {
                                id: chunk_id.to_string(),
                                file_id: file_id.to_string(),
                                key: key.to_string(),
                                size: chunk_size,
                                stored_bytes: stored_size,
                                encoding: encoding.clone(),
                                sha256: hex(&chunk_sha),
                                local_copy: true,
                                archive_object_key: None,
                                archive_wrapped_key: None,
                            };
                            let plain = crate::archive::verify_chunk_payload(&entry, &stored)?;
                            chunk_actual = plain.len() as u64;
                            chunk_digest.update(&plain);
                            digest.update(&plain);
                        }
                        "archive" => {
                            let access=archive.ok_or_else(||BackupError::new(ErrorKind::SourceDamaged,"chunked archive versions require the configured archive provider and server key"))?;
                            let entry = crate::manifest::ChunkEntry {
                                id: chunk_id.to_string(),
                                file_id: file_id.to_string(),
                                key,
                                size: chunk_size,
                                stored_bytes: stored_size,
                                encoding,
                                sha256: hex(&chunk_sha),
                                local_copy: false,
                                archive_object_key: archive_key,
                                archive_wrapped_key: wrapped.map(|bytes| hex(&bytes)),
                            };
                            let stored = crate::archive::read_archive_chunk(
                                access.store,
                                access.crypto,
                                &entry,
                            )
                            .await?;
                            let plain = crate::archive::verify_chunk_payload(&entry, &stored)?;
                            chunk_actual = plain.len() as u64;
                            chunk_digest.update(&plain);
                            digest.update(&plain);
                        }
                        _ => {
                            return Err(BackupError::new(
                                ErrorKind::SourceDamaged,
                                "a chunk has an unknown storage tier",
                            ));
                        }
                    }
                    if chunk_actual != chunk_size
                        || <[u8; 32]>::from(chunk_digest.finalize()) != chunk_sha.as_slice()
                    {
                        return Err(BackupError::new(
                            ErrorKind::SourceDamaged,
                            format!("chunk {chunk_id} does not match its recorded checksum"),
                        ));
                    }
                    size = size.checked_add(chunk_size).ok_or_else(|| {
                        BackupError::new(ErrorKind::Integrity, "assembled version size overflow")
                    })?;
                    offset = offset
                        .checked_add(i64::try_from(chunk_size).map_err(|_| {
                            BackupError::new(ErrorKind::Integrity, "chunk offset overflow")
                        })?)
                        .ok_or_else(|| {
                            BackupError::new(ErrorKind::Integrity, "chunk offset overflow")
                        })?;
                    next_ordinal = next_ordinal.checked_add(1).ok_or_else(|| {
                        BackupError::new(ErrorKind::Integrity, "chunk ordinal overflow")
                    })?;
                }
            }
            if size != expected_size
                || <[u8; 32]>::from(digest.finalize()) != expected_sha.as_slice()
            {
                return Err(BackupError::new(
                    ErrorKind::SourceDamaged,
                    format!(
                        "chunk assembly for version {version_id} does not match its full version checksum"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Copies the objects listed in `blobs.jsonl` into `blobs/`, checking size and
/// SHA-256 against the database record while reading.
async fn copy_blobs(store: &LocalStore, out: &Path) -> Result<(u64, u64), BackupError> {
    let list = tokio::fs::File::open(out.join(BLOB_INDEX_FILE))
        .await
        .map_err(|error| BackupError::io("cannot read the blob list", &error))?;
    let mut reader = tokio::io::BufReader::new(list);
    let mut line = Vec::new();
    let (mut count, mut total) = (0_u64, 0_u64);
    loop {
        line.clear();
        let got = (&mut reader)
            .take(MAX_BLOB_LINE_BYTES as u64 + 2)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|error| BackupError::io("cannot read the blob list", &error))?;
        if got == 0 {
            return Ok((count, total));
        }
        if line.pop() != Some(b'\n') {
            return Err(BackupError::new(
                ErrorKind::Format,
                "the blob list is damaged",
            ));
        }
        let entry = BlobEntry::parse_line(&line)?;
        if entry.local_copy && entry.layout == "whole" {
            copy_one(store, out, &entry).await?;
            count += 1;
            total = total.checked_add(entry.size).ok_or_else(|| {
                BackupError::new(ErrorKind::Integrity, "local blob byte count overflow")
            })?;
        }
    }
}

async fn copy_chunks(store: &LocalStore, out: &Path) -> Result<(u64, u64), BackupError> {
    make_dir(&out.join(CHUNKS_DIR), "the chunks directory")?;
    let list = tokio::fs::File::open(out.join(CHUNK_INDEX_FILE))
        .await
        .map_err(|error| BackupError::io("cannot read the chunk list", &error))?;
    let mut reader = tokio::io::BufReader::new(list);
    let mut line = Vec::new();
    let (mut count, mut total) = (0_u64, 0_u64);
    loop {
        line.clear();
        let got = (&mut reader)
            .take(MAX_BLOB_LINE_BYTES as u64 + 2)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|error| BackupError::io("cannot read the chunk list", &error))?;
        if got == 0 {
            return Ok((count, total));
        }
        if line.pop() != Some(b'\n') {
            return Err(BackupError::new(
                ErrorKind::Format,
                "the chunk list is damaged",
            ));
        }
        let entry = crate::manifest::ChunkEntry::parse_line(&line)?;
        if entry.local_copy {
            copy_chunk_one(store, out, &entry).await?;
            count += 1;
            total = total.checked_add(entry.stored_len()).ok_or_else(|| {
                BackupError::new(ErrorKind::Integrity, "chunk byte count overflow")
            })?;
        }
    }
}

async fn copy_chunk_one(
    store: &LocalStore,
    out: &Path,
    entry: &crate::manifest::ChunkEntry,
) -> Result<(), BackupError> {
    let key = ObjectKey::parse(&entry.key)
        .map_err(|_| BackupError::new(ErrorKind::Format, "a chunk key is invalid"))?;
    let mut source = store.open_read(&key, None).await.map_err(|error| {
        if matches!(error, StorageError::NotFound) {
            BackupError::new(
                ErrorKind::SourceDamaged,
                format!("chunk {} is missing from the store", entry.id),
            )
        } else {
            error.into()
        }
    })?;
    if source.remaining() != entry.stored_len() {
        return Err(BackupError::new(
            ErrorKind::SourceDamaged,
            format!("chunk {} has a different size", entry.id),
        ));
    }
    let relative = crate::manifest::chunk_file(&entry.id);
    let capacity = usize::try_from(entry.stored_len()).map_err(|_| {
        BackupError::new(
            ErrorKind::SourceDamaged,
            "chunk stored size exceeds addressable memory",
        )
    })?;
    let mut stored = Vec::with_capacity(capacity);
    while let Some(block) = source.next_chunk().await? {
        if stored.len().saturating_add(block.len()) > capacity {
            return Err(BackupError::new(
                ErrorKind::SourceDamaged,
                "chunk stream exceeds its recorded stored size",
            ));
        }
        stored.extend_from_slice(&block);
    }
    let _ = crate::archive::verify_chunk_payload(entry, &stored)?;
    let mut file = Out::create(&out.join(&relative), &relative)?;
    file.write(&stored).await?;
    let sum = file.finish().await?;
    if sum.len != entry.stored_len() {
        return Err(BackupError::new(
            ErrorKind::SourceDamaged,
            format!("chunk {} does not match its recorded checksum", entry.id),
        ));
    }
    Ok(())
}

async fn copy_one(store: &LocalStore, out: &Path, entry: &BlobEntry) -> Result<(), BackupError> {
    let damaged = |what: &str| {
        BackupError::new(
            ErrorKind::SourceDamaged,
            format!(
                "object {} {what}; no complete backup can be made",
                entry.key
            ),
        )
    };
    let key = ObjectKey::parse(&entry.key)
        .map_err(|_| BackupError::new(ErrorKind::Format, "a blob key is invalid"))?;
    let mut source = match store.open_read(&key, None).await {
        Ok(source) => source,
        Err(StorageError::NotFound) => return Err(damaged("is missing from the store")),
        Err(error) => return Err(error.into()),
    };
    if source.remaining() != entry.size {
        return Err(damaged("has a different size than the database records"));
    }
    let relative = blob_file(&entry.key);
    if let Some(shard) = Path::new(&relative).parent() {
        make_dir(&out.join(shard), "a blob directory")?;
    }
    let mut file = Out::create(&out.join(&relative), &relative)?;
    loop {
        match source.next_chunk().await {
            Ok(Some(chunk)) => file.write(&chunk).await?,
            Ok(None) => break,
            Err(StorageError::NotFound) => return Err(damaged("is missing from the store")),
            Err(error) => return Err(error.into()),
        }
    }
    let written = file.finish().await?;
    if written.len != entry.size || hex(&written.sha256) != entry.sha256 {
        return Err(damaged("does not match its recorded checksum"));
    }
    Ok(())
}
