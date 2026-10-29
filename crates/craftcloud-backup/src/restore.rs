//! `backup restore`: loads a verified backup into an empty database and an
//! empty store.
//!
//! Order of work (nothing is written before the first four steps pass):
//!
//! 1. `verify` the whole backup directory.
//! 2. Take the maintenance advisory lock for the whole restore. A second
//!    restore (or a maintenance run, or a backup) cannot run at the same time;
//!    it fails with `Busy` and never touches what this restore writes.
//! 3. The target must be empty: no object and no marker in the store; a
//!    database without tables, or one migrated to exactly the backup's
//!    migration version whose tables are all empty. The connected database role
//!    must own `audit_events` (see below). Nothing is ever overwritten.
//! 4. A database without tables is migrated to the backup's migration version
//!    (and the runtime role is granted, when one is configured).
//! 5. Blobs are streamed into the store under their original keys; every
//!    object is checked against its SHA-256 while it is written, and the blob
//!    list is hashed while it is read and compared with the manifest.
//! 6. One transaction loads the tables with `COPY ... FROM STDIN` in
//!    foreign-key order (every table file is hashed while it streams and
//!    compared with the manifest before the commit), cleans up unfinished
//!    uploads, records the `backup.restored` audit event and inserts the
//!    `storage_binding` row with a fresh store identity at epoch 0. The marker
//!    file with the same identity is written just before the commit.
//!
//! **What a failure leaves behind.** Until the commit nothing is visible: a
//! failure rolls the transaction back, removes the marker (only if this restore
//! wrote it) and removes the objects of step 5 (tracked by this restore, not
//! re-read from the backup). After the commit there is no further step, so a
//! successful restore is complete: tables, binding and marker agree. The only
//! failure that cannot be undone by the program is a marker file that cannot be
//! removed after a failed commit; the error then says so and names the file.
//!
//! **Sessions.** The `sessions` table is not part of a backup, so every
//! session is invalid after a restore and everybody has to log in again.
//! Password reset links are not restored either.
//!
//! **Unfinished uploads.** An upload that was in progress when the backup was
//! taken has no bytes in the backup. Its session is set to `aborted`, its
//! staged blob to `orphaned` (collected by the next maintenance run, which also
//! releases the bytes still reserved for those blobs); no other reservation
//! remains.
//!
//! **Audit log timestamps.** The audit insert guard (migration 0012) rewrites
//! `created_at`, `last_at` and `event_count` for every role except the table
//! owner. A restore through another role would silently change the restored
//! audit log, so it is refused unless the connected role owns `audit_events`.
//!
//! **Schema.** The backup is restored at its own migration version, with the
//! tables that exist at that version. A backup of an older schema is restored
//! as it is; run `craftcloud-server migrate` afterwards to bring it forward
//! (restore, then migrate). A backup of a newer schema than this server knows
//! is refused.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use craftcloud_db::PgPool;
use craftcloud_db::audit::{self, AuditCtx, Detail, NewEvent};
use craftcloud_db::maintenance as db;
use craftcloud_storage::{LocalStore, MarkerInfo, ObjectKey, ObjectStore, StorageError, WriteSink};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use uuid::Uuid;

use crate::error::{BackupError, ErrorKind};
use crate::manifest::{BlobEntry, Manifest, blob_file, hex, parse_sha256};
use crate::tables::{backup_tables_for, excluded_tables_for};
use crate::verify::{BLOCK, for_each_blob, verify};

/// Points of a running restore at which other work can be interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePhase {
    /// The backup is verified, the lock is held and the target is checked;
    /// nothing has been written.
    Verified,
    /// All objects are in the store; the database is not loaded yet.
    BlobsWritten,
}

/// A callback run at each [`RestorePhase`], awaited before the restore
/// continues. Tests use it to change the backup between `verify` and the
/// load; the server passes none.
pub type RestoreHook =
    Arc<dyn Fn(RestorePhase) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Parameters of [`restore`].
#[derive(Clone, Default)]
pub struct RestoreOptions<'a> {
    /// A manifest digest recorded elsewhere (for example printed by
    /// `backup create`); the restore refuses a backup whose manifest differs.
    pub expected_manifest_sha256: Option<&'a str>,
    /// The runtime role to grant data privileges when the restore has to run
    /// the migrations itself (`CRAFTCLOUD_DATABASE_RUNTIME_ROLE`).
    pub runtime_role: Option<&'a str>,
    /// Called at each [`RestorePhase`]; `None` in production.
    pub on_phase: Option<RestoreHook>,
}

/// What a restore did.
#[derive(Debug, Clone)]
pub struct RestoreReport {
    /// The restored backup's manifest.
    pub manifest: Manifest,
    /// Rows loaded.
    pub rows: u64,
    /// Objects written.
    pub blobs: u64,
    /// Bytes of the written objects.
    pub blob_bytes: u64,
    /// Unfinished upload sessions that were aborted.
    pub aborted_uploads: u64,
    /// The new identity of the store (and of its binding).
    pub store_id: Uuid,
    /// Whether the database is at an older schema than this server (run
    /// `craftcloud-server migrate`).
    pub needs_migration: bool,
    /// Wall-clock time of the restore, including the verification.
    pub duration: Duration,
}

/// The objects this restore wrote, kept so that a failure removes exactly
/// those (as 128-bit numbers: 16 bytes per key).
#[derive(Default)]
struct Written {
    keys: Vec<u128>,
    bytes: u64,
}

impl Written {
    fn count(&self) -> u64 {
        self.keys.len() as u64
    }
}

/// Restores the backup in `dir` into the empty database behind `pool` (which
/// must be connected as the owner of the tables, so the audit log keeps its
/// timestamps) and the empty store.
///
/// # Errors
///
/// [`BackupError`]: an integrity or format problem of the backup, a target
/// that is not empty, another restore, backup or maintenance run holding the
/// lock, or a database or storage failure. After an error the database holds
/// no restored rows, binding or marker and the store no restored objects.
pub async fn restore(
    pool: &PgPool,
    store: &LocalStore,
    dir: &Path,
    options: &RestoreOptions<'_>,
) -> Result<RestoreReport, BackupError> {
    let started = Instant::now();

    let (check_dir, expected) = (
        dir.to_path_buf(),
        options.expected_manifest_sha256.map(str::to_owned),
    );
    let verified = tokio::task::spawn_blocking(move || verify(&check_dir, expected.as_deref()))
        .await
        .map_err(|_| BackupError::new(ErrorKind::Io, "the verification task failed"))??;

    // Exclusive for the whole restore: a second restore must not find our
    // half-written objects and "compensate" them away.
    let Some(lock) = db::try_lock(pool).await? else {
        return Err(BackupError::new(
            ErrorKind::Busy,
            "another restore, a backup or a maintenance run holds the lock; try again when it has finished",
        ));
    };
    let result = restore_locked(pool, store, dir, options, verified.manifest, started).await;
    if let Err(error) = lock.release().await {
        tracing::warn!(%error, "the restore lock could not be released explicitly; the connection is closed instead");
    }
    result
}

async fn notify(options: &RestoreOptions<'_>, phase: RestorePhase) {
    if let Some(hook) = &options.on_phase {
        hook(phase).await;
    }
}

async fn restore_locked(
    pool: &PgPool,
    store: &LocalStore,
    dir: &Path,
    options: &RestoreOptions<'_>,
    manifest: Manifest,
    started: Instant,
) -> Result<RestoreReport, BackupError> {
    ensure_store_empty(store).await?;
    prepare_database(pool, &manifest, options.runtime_role).await?;
    notify(options, RestorePhase::Verified).await;

    let written = write_blobs(store, dir, &manifest).await?;
    notify(options, RestorePhase::BlobsWritten).await;

    // A fresh identity for the restored pair: it must not look like the
    // instance the backup came from (that one may still be running).
    let store_id = Uuid::new_v4();
    let (rows, aborted_uploads) = match load_tables(pool, store, dir, &manifest, store_id).await {
        Ok(done) => done,
        Err(error) => {
            remove_blobs(store, &written).await;
            return Err(error);
        }
    };

    let latest = craftcloud_db::migrate::embedded_versions()
        .into_iter()
        .max()
        .unwrap_or(0);
    Ok(RestoreReport {
        needs_migration: manifest.migration_version < latest,
        manifest,
        rows,
        blobs: written.count(),
        blob_bytes: written.bytes,
        aborted_uploads,
        store_id,
        duration: started.elapsed(),
    })
}

async fn ensure_store_empty(store: &LocalStore) -> Result<(), BackupError> {
    let not_empty = |what: &str| {
        BackupError::new(
            ErrorKind::TargetNotEmpty,
            format!(
                "the target store already has {what}; a restore only writes into an empty storage directory"
            ),
        )
    };
    if store.read_marker_info().await?.is_some() {
        return Err(not_empty("a marker file"));
    }
    if !store.list_objects(None, 1).await?.is_empty() {
        return Err(not_empty("objects"));
    }
    if !store.list_temp(None, 1).await?.is_empty() {
        return Err(not_empty("unfinished temporary files"));
    }
    Ok(())
}

/// Brings the database to the backup's migration version and checks that
/// every table of that version is empty and that the connected role owns the
/// audit log table.
async fn prepare_database(
    pool: &PgPool,
    manifest: &Manifest,
    runtime_role: Option<&str>,
) -> Result<(), BackupError> {
    let has_history: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    let migrator = craftcloud_db::migrate::migrator_up_to(manifest.migration_version);
    if has_history {
        craftcloud_db::migrate::verify_with(pool, &migrator)
            .await
            .map_err(|error| {
                BackupError::new(
                    ErrorKind::Schema,
                    format!(
                        "the target database is not at the backup's migration version {} ({error}); \
                         use an empty database or one migrated to exactly that version",
                        manifest.migration_version
                    ),
                )
            })?;
    } else {
        let tables: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")
                .fetch_one(pool)
                .await?;
        if tables > 0 {
            return Err(BackupError::new(
                ErrorKind::TargetNotEmpty,
                "the target database already has tables but no migration history; use an empty database",
            ));
        }
        craftcloud_db::migrate::run_with(pool, &migrator).await?;
        if let Some(role) = runtime_role {
            craftcloud_db::migrate::grant_runtime_role(pool, role)
                .await
                .map_err(|error| {
                    BackupError::new(
                        ErrorKind::Schema,
                        format!("granting the runtime role: {error}"),
                    )
                })?;
        }
    }

    // The audit insert guard rewrites timestamps and counters of rows inserted
    // by any role but the table owner: refuse instead of changing the log.
    let (owner, current): (String, String) = sqlx::query_as(
        "SELECT pg_catalog.pg_get_userbyid(c.relowner)::text, current_user::text \
         FROM pg_catalog.pg_class c WHERE c.oid = to_regclass('public.audit_events')",
    )
    .fetch_one(pool)
    .await?;
    if owner != current {
        return Err(BackupError::new(
            ErrorKind::Schema,
            format!(
                "the restore is connected as role {current}, but the table owner is {owner}: the audit log guard \
                 would rewrite the timestamps and counters of the restored audit events. Restore with the owner role \
                 (CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE)"
            ),
        ));
    }

    // Everything must be empty, the excluded tables included: a restore never
    // merges into data.
    let names = backup_tables_for(manifest.migration_version)?
        .into_iter()
        .map(|t| t.name)
        .chain(excluded_tables_for(manifest.migration_version));
    for name in names {
        let has_rows: bool =
            sqlx::query_scalar(&format!("SELECT EXISTS (SELECT 1 FROM \"{name}\")"))
                .fetch_one(pool)
                .await?;
        if has_rows {
            return Err(BackupError::new(
                ErrorKind::TargetNotEmpty,
                format!(
                    "the target database is not empty (table {name} has rows); a restore never overwrites data"
                ),
            ));
        }
    }
    Ok(())
}

/// Streams every listed object into the store under its original key. The
/// blob list is hashed while it is read and compared with the manifest, so a
/// list that changed after `verify` is caught here.
async fn write_blobs(
    store: &LocalStore,
    dir: &Path,
    manifest: &Manifest,
) -> Result<Written, BackupError> {
    // The blob list is read on a blocking thread, one entry at a time, and
    // handed over through a small channel so that async store calls can run.
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<BlobEntry, BackupError>>(8);
    let list_dir = dir.to_path_buf();
    let producer = tokio::task::spawn_blocking(move || {
        let outcome = for_each_blob(&list_dir, |entry| {
            sender
                .blocking_send(Ok(entry.clone()))
                .map_err(|_| BackupError::new(ErrorKind::Io, "the restore was cancelled"))
        });
        if let Err(error) = &outcome {
            let _ = sender.blocking_send(Err(error.clone()));
        }
        outcome
    });

    let mut written = Written::default();
    let mut failure: Option<BackupError> = None;
    while let Some(next) = receiver.recv().await {
        let entry = match next {
            Ok(entry) => entry,
            Err(error) => {
                failure = Some(error);
                break;
            }
        };
        if let Err(error) = write_one(store, dir, &entry).await {
            failure = Some(error);
            break;
        }
        match u128::from_str_radix(&entry.key, 16) {
            Ok(key) => written.keys.push(key),
            Err(_) => {
                failure = Some(BackupError::new(ErrorKind::Format, "a blob key is invalid"));
                break;
            }
        }
        written.bytes = match written.bytes.checked_add(entry.size) {
            Some(total) => total,
            None => {
                failure = Some(BackupError::new(
                    ErrorKind::Integrity,
                    "the blob sizes overflow",
                ));
                break;
            }
        };
    }
    drop(receiver);
    let produced = producer.await;
    if failure.is_none() {
        failure = match produced {
            Ok(Ok(sum)) => {
                if hex(&sum.sha256) != manifest.blobs.sha256
                    || sum.count != manifest.blobs.count
                    || sum.total_bytes != manifest.blobs.total_bytes
                    || sum.count != written.count()
                {
                    Some(BackupError::new(
                        ErrorKind::Integrity,
                        "blobs.jsonl changed after verification (it does not match the manifest)",
                    ))
                } else {
                    None
                }
            }
            Ok(Err(error)) => Some(error),
            Err(_) => Some(BackupError::new(
                ErrorKind::Io,
                "reading the blob list failed",
            )),
        };
    }
    match failure {
        None => Ok(written),
        Some(error) => {
            remove_blobs(store, &written).await;
            Err(error)
        }
    }
}

async fn write_one(store: &LocalStore, dir: &Path, entry: &BlobEntry) -> Result<(), BackupError> {
    let key = ObjectKey::parse(&entry.key)
        .map_err(|_| BackupError::new(ErrorKind::Format, "a blob key is invalid"))?;
    let digest = parse_sha256(&entry.sha256)
        .ok_or_else(|| BackupError::new(ErrorKind::Format, "a blob checksum is invalid"))?;
    let mut file = tokio::fs::File::open(dir.join(blob_file(&entry.key)))
        .await
        .map_err(|error| BackupError::io("cannot open a blob file", &error))?;
    let mut temp = store.begin_write(entry.size).await?;
    let mut buffer = vec![0_u8; BLOCK];
    loop {
        let got = match file.read(&mut buffer).await {
            Ok(got) => got,
            Err(error) => {
                store.abort(temp).await;
                return Err(BackupError::io("cannot read a blob file", &error));
            }
        };
        if got == 0 {
            break;
        }
        if let Err(error) = temp.write(buffer.get(..got).unwrap_or_default()).await {
            // A failed write has already removed the temporary object.
            return Err(error.into());
        }
    }
    match store.finalize_as(temp, key, entry.size, &digest).await {
        Ok(_) => Ok(()),
        Err(StorageError::ChecksumMismatch | StorageError::LengthMismatch) => {
            Err(BackupError::new(
                ErrorKind::Integrity,
                format!(
                    "blob {} does not match its checksum while being written",
                    entry.key
                ),
            ))
        }
        Err(error) => Err(error.into()),
    }
}

/// Removes the objects this restore wrote (compensation after a failure; best
/// effort). The store was empty and the lock is held, so these are the only
/// objects that can be there.
async fn remove_blobs(store: &LocalStore, written: &Written) {
    for key in &written.keys {
        if let Ok(key) = ObjectKey::parse(&format!("{key:032x}"))
            && let Err(error) = store.delete(&key).await
        {
            tracing::warn!(%error, "an object written by the failed restore could not be removed");
        }
    }
}

/// Reads a file and hashes what passes through.
struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    len: u64,
}

impl<R: AsyncRead + Unpin> AsyncRead for HashingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let fresh = buf.filled().get(before..).unwrap_or_default();
                this.hasher.update(fresh);
                this.len += fresh.len() as u64;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// One transaction: all tables, the fix-ups, the audit event, the binding.
/// The marker file is written last before the commit and removed again if the
/// commit fails, so the committed database and the store always agree.
async fn load_tables(
    pool: &PgPool,
    store: &LocalStore,
    dir: &Path,
    manifest: &Manifest,
    store_id: Uuid,
) -> Result<(u64, u64), BackupError> {
    let specs = backup_tables_for(manifest.migration_version)?;
    let mut tx = pool.begin().await?;
    let mut rows = 0_u64;
    for (spec, entry) in specs.iter().zip(&manifest.tables) {
        let target = match spec.deferred_column {
            Some(_) => format!("_restore_{}", spec.name),
            None => spec.name.to_owned(),
        };
        if let Some(column) = spec.deferred_column {
            // Load into a scratch table, remember the column that closes a
            // cycle, clear it, and insert; the column is set after all
            // tables are in.
            let name = spec.name;
            sqlx::query(&format!(
                "CREATE TEMP TABLE \"{target}\" ON COMMIT DROP AS SELECT * FROM \"{name}\" WITH NO DATA"
            ))
            .execute(&mut *tx)
            .await?;
            copy_in(&mut tx, dir, entry, &target).await?;
            sqlx::query(&format!(
                "CREATE TEMP TABLE \"{target}_ref\" ON COMMIT DROP AS \
                 SELECT id, \"{column}\" FROM \"{target}\" WHERE \"{column}\" IS NOT NULL"
            ))
            .execute(&mut *tx)
            .await?;
            sqlx::query(&format!("UPDATE \"{target}\" SET \"{column}\" = NULL"))
                .execute(&mut *tx)
                .await?;
            sqlx::query(&format!(
                "INSERT INTO \"{name}\" SELECT * FROM \"{target}\""
            ))
            .execute(&mut *tx)
            .await?;
        } else {
            copy_in(&mut tx, dir, entry, &target).await?;
        }
        rows = rows
            .checked_add(entry.rows)
            .ok_or_else(|| BackupError::new(ErrorKind::Integrity, "the row counts overflow"))?;
    }
    for spec in &specs {
        if let Some(column) = spec.deferred_column {
            sqlx::query(&format!(
                "UPDATE \"{name}\" t SET \"{column}\" = r.\"{column}\" \
                 FROM \"_restore_{name}_ref\" r WHERE t.id = r.id",
                name = spec.name
            ))
            .execute(&mut *tx)
            .await?;
        }
    }

    // Unfinished uploads have no bytes in the backup.
    let aborted = sqlx::query(
        "UPDATE upload_sessions SET state = 'aborted', updated_at = now(), version = version + 1 \
         WHERE state IN ('created', 'receiving', 'stored')",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query("UPDATE blobs SET state = 'orphaned' WHERE state = 'staged'")
        .execute(&mut *tx)
        .await?;
    // No session holds a reservation any more; the orphaned blobs (their
    // objects are missing and are collected by the next maintenance run) are
    // the only reserved bytes, as the usage reconciliation derives them.
    sqlx::query(
        "UPDATE workspaces w SET reserved_bytes = COALESCE((SELECT sum(b.size_bytes) FROM blobs b          WHERE b.workspace_id = w.id AND b.state = 'orphaned'), 0)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE organizations o SET reserved_bytes = COALESCE((SELECT sum(w.reserved_bytes)          FROM workspaces w WHERE w.org_id = o.id), 0)",
    )
    .execute(&mut *tx)
    .await?;

    let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    audit::record_in(
        &mut tx,
        &AuditCtx::system(),
        &NewEvent::new("backup.restored")
            .detail("rows", Detail::Count(count(rows)))
            .detail("blobs", Detail::Count(count(manifest.blobs.count)))
            .detail("aborted_uploads", Detail::Count(count(aborted)))
            .detail("migration", Detail::Count(manifest.migration_version)),
    )
    .await?;

    // The binding of the new pair. A server that bound the store meanwhile
    // makes this insert fail (primary key): the restore then fails cleanly.
    sqlx::query(
        "INSERT INTO storage_binding (id, store_id, bound_at, epoch) VALUES (true, $1, now(), 0)",
    )
    .bind(store_id)
    .execute(&mut *tx)
    .await?;

    // The marker goes to disk before the commit: if it cannot be written, the
    // transaction rolls back; if the commit fails, the marker is removed.
    store
        .write_marker_info(MarkerInfo { store_id, epoch: 0 }, false)
        .await?;
    if let Err(error) = tx.commit().await {
        if let Err(remove) = store.remove_marker().await {
            return Err(BackupError::new(
                ErrorKind::Binding,
                format!(
                    "the restore failed to commit and the store marker file craftcloud-store.json could not be removed ({remove}): delete it before trying again"
                ),
            ));
        }
        return Err(error.into());
    }
    Ok((rows, aborted))
}

/// `COPY ... FROM STDIN` of one table file. The file is hashed while it
/// streams; length, checksum and row count must match the manifest before the
/// caller may commit (a file that was swapped after `verify` fails here).
async fn copy_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dir: &Path,
    entry: &crate::manifest::TableEntry,
    table: &str,
) -> Result<(), BackupError> {
    let relative = entry.file.as_str();
    let file = tokio::fs::File::open(dir.join(relative))
        .await
        .map_err(|error| BackupError::io(&format!("cannot open {relative}"), &error))?;
    let mut reader = HashingReader {
        inner: file,
        hasher: Sha256::new(),
        len: 0,
    };
    let mut copy = tx
        .copy_in_raw(&format!("COPY \"{table}\" FROM STDIN"))
        .await?;
    // Streams in blocks; a file read error aborts the COPY (and the transaction).
    if let Err(error) = copy.read_from(&mut reader).await {
        let _ = copy.abort("restore failed").await;
        return Err(error.into());
    }
    let loaded = copy.finish().await?;
    let digest: [u8; 32] = reader.hasher.finalize().into();
    if reader.len != entry.bytes || hex(&digest) != entry.sha256 {
        return Err(BackupError::new(
            ErrorKind::Integrity,
            format!("{relative} changed after verification (it does not match the manifest)"),
        ));
    }
    if loaded != entry.rows {
        return Err(BackupError::new(
            ErrorKind::Integrity,
            format!(
                "{relative}: loaded {loaded} rows, the manifest says {}",
                entry.rows
            ),
        ));
    }
    Ok(())
}
