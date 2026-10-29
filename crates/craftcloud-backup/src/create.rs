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

use crate::error::{BackupError, ErrorKind};
use crate::manifest::{
    BLOB_INDEX_FILE, BLOBS_DIR, BlobEntry, BlobIndex, FORMAT, FORMAT_VERSION, MANIFEST_FILE,
    MANIFEST_SUM_FILE, MAX_BLOB_LINE_BYTES, Manifest, StoreInfo, TABLES_DIR, TableEntry, blob_file,
    hex, table_file,
};
use crate::tables::{backup_tables_for, excluded_tables_for};

/// Size of the write buffer in front of every output file.
const WRITE_BUFFER: usize = 256 * 1024;
/// Rows of the blob list read from the snapshot at once.
const BLOB_PAGE: i64 = 1000;

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
    let started = Instant::now();
    let created_dir = prepare_output(out)?;
    match create_inner(pool, store, out, options, started).await {
        Ok(mut report) => {
            let event = NewEvent::new("backup.created")
                .detail(
                    "tables",
                    Detail::Count(count(report.manifest.tables.len() as u64)),
                )
                .detail("rows", Detail::Count(count(report.rows)))
                .detail("blobs", Detail::Count(count(report.blobs)))
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
            Err(error)
        }
    }
}

/// Removes what a backup writes at the top level of its output directory.
fn remove_own_entries(out: &Path) {
    for name in [TABLES_DIR, BLOBS_DIR] {
        let _ = std::fs::remove_dir_all(out.join(name));
    }
    for name in [BLOB_INDEX_FILE, MANIFEST_FILE, MANIFEST_SUM_FILE] {
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
    let result = locked(pool, store, out, options, started, store_id, epoch).await;
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

async fn locked(
    pool: &PgPool,
    store: &LocalStore,
    out: &Path,
    options: &CreateOptions<'_>,
    started: Instant,
    store_id: Uuid,
    epoch: i64,
) -> Result<CreateReport, BackupError> {
    notify(options, Phase::LockHeld).await;
    make_dir(&out.join(TABLES_DIR), "the tables directory")?;
    make_dir(&out.join(BLOBS_DIR), "the blobs directory")?;

    let snapshot = snapshot(pool, out).await?;
    notify(options, Phase::SnapshotTaken).await;
    let (blobs, blob_bytes) = copy_blobs(store, out).await?;
    notify(options, Phase::BlobsCopied).await;
    if blobs != snapshot.index.count || blob_bytes != snapshot.index.total_bytes {
        return Err(BackupError::new(
            ErrorKind::Integrity,
            "the copied blobs do not match the blob list",
        ));
    }

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
        excluded_tables: excluded_tables_for(snapshot.migration_version)
            .iter()
            .map(|t| (*t).to_owned())
            .collect(),
        unavailable_version_blobs: snapshot.unavailable,
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
    Ok(CreateReport {
        manifest,
        manifest_sha256: digest,
        rows,
        blobs,
        blob_bytes,
        duration: started.elapsed(),
        audit_recorded: false,
    })
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
    unavailable: u64,
}

/// Exports every table and the list of committed blobs from one snapshot.
async fn snapshot(pool: &PgPool, out: &Path) -> Result<Snapshot, BackupError> {
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
        let mut stream = tx
            .copy_out_raw(&format!("COPY \"{}\" TO STDOUT", spec.name))
            .await?;
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

    let mut list = Out::create(&out.join(BLOB_INDEX_FILE), BLOB_INDEX_FILE)?;
    let (mut blob_count, mut blob_total) = (0_u64, 0_u64);
    let mut last = String::new();
    let mut previous: Option<String> = None;
    loop {
        let page: Vec<(Uuid, String, i64, Vec<u8>)> = sqlx::query_as(
            "SELECT id, object_key, size_bytes, sha256 FROM blobs \
             WHERE state = 'committed' AND object_key COLLATE \"C\" > $1 \
             ORDER BY object_key COLLATE \"C\" LIMIT $2",
        )
        .bind(&last)
        .bind(BLOB_PAGE)
        .fetch_all(&mut *tx)
        .await?;
        let Some((_, newest, _, _)) = page.last() else {
            break;
        };
        last.clone_from(newest);
        for (id, key, size, sha256) in &page {
            let size = u64::try_from(*size).map_err(|_| {
                BackupError::new(ErrorKind::SourceDamaged, "a blob has a negative size")
            })?;
            let entry = BlobEntry {
                id: id.to_string(),
                key: key.clone(),
                size,
                sha256: hex(sha256),
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
        }
    }
    let written = list.finish().await?;
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
        unavailable: u64::try_from(unavailable).unwrap_or(0),
    })
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
        copy_one(store, out, &entry).await?;
        count += 1;
        total += entry.size;
    }
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
