//! Explicit recovery inventory for backup directories outside the database.

use std::collections::BTreeSet;
use std::path::PathBuf;
use uuid::Uuid;

use craftcloud_db::{PgPool, maintenance as db};

use crate::archive::ArchiveAccess;
use crate::blob_stream::blob_entries;
use crate::create::register_archive_pins;
use crate::error::{BackupError, ErrorKind};
use crate::verify::{VerifyReport, verify, verify_with_archive};

/// Result of inventorying the operator-supplied complete backup set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryReport {
    /// Directories validated and represented in the pin registry.
    pub backups: u64,
    /// Durable archive references retained by those backups.
    pub archive_references: u64,
}

/// Explicitly releases pins for a backup already deleted by its operator.
/// The ID and manifest digest must both match the durable registry entry.
pub async fn release_backup_pins(
    pool: &PgPool,
    backup_id: Uuid,
    manifest_sha256: &str,
    operator_confirmed_deleted: bool,
) -> Result<u64, BackupError> {
    if !operator_confirmed_deleted {
        return Err(BackupError::new(
            ErrorKind::Format,
            "release requires explicit confirmation that the matching backup was deleted",
        ));
    }
    if !crate::manifest::is_sha256_hex(manifest_sha256) {
        return Err(BackupError::new(
            ErrorKind::Format,
            "the manifest digest must be 64 lowercase hex characters",
        ));
    }
    let Some(lock) = db::try_lock(pool).await? else {
        return Err(BackupError::new(
            ErrorKind::Busy,
            "another restore, backup or maintenance run holds the lock",
        ));
    };
    let result = async {
        db::archive_backup_release(pool, backup_id, manifest_sha256)
            .await?
            .ok_or_else(|| {
                BackupError::new(
                    ErrorKind::Integrity,
                    "no pin registration matches that backup id and manifest digest",
                )
            })
    }
    .await;
    if let Err(error) = lock.release().await {
        tracing::warn!(%error, "the archive release lock could not be released explicitly; the connection is closed instead");
    }
    result
}

/// Verifies and pins every supplied backup directory, then opens archive GC.
///
/// The function holds the maintenance advisory lock. The operator must supply
/// every retained external backup. Explicit confirmation is required because
/// a partial directory list could authorize deletion of objects referenced
/// by an omitted backup. Any failed check or
/// database update leaves `inventory_complete=false`; pins already imported
/// are safe to retry and are not removed implicitly.
pub async fn inventory_backups(
    pool: &PgPool,
    directories: &[PathBuf],
    archive: Option<ArchiveAccess<'_>>,
    operator_confirmed_complete: bool,
) -> Result<InventoryReport, BackupError> {
    let Some(lock) = db::try_lock(pool).await? else {
        return Err(BackupError::new(
            ErrorKind::Busy,
            "another restore, backup or maintenance run holds the lock",
        ));
    };
    let result = inventory_locked(pool, directories, archive, operator_confirmed_complete).await;
    if let Err(error) = lock.release().await {
        tracing::warn!(%error, "the archive inventory lock could not be released explicitly; the connection is closed instead");
    }
    result
}

async fn inventory_locked(
    pool: &PgPool,
    directories: &[PathBuf],
    archive: Option<ArchiveAccess<'_>>,
    operator_confirmed_complete: bool,
) -> Result<InventoryReport, BackupError> {
    if !operator_confirmed_complete {
        return Err(BackupError::new(
            ErrorKind::Format,
            "archive inventory requires explicit confirmation that the supplied directory set is complete",
        ));
    }
    let mut seen = BTreeSet::new();
    for directory in directories {
        let normalized = std::fs::canonicalize(directory)
            .map_err(|error| BackupError::io("cannot resolve a backup directory", &error))?;
        if !seen.insert(normalized) {
            return Err(BackupError::new(
                ErrorKind::Format,
                "the archive inventory lists a backup directory more than once",
            ));
        }
    }

    db::archive_gc_mark_inventory_incomplete(pool).await?;
    let mut checked: Vec<(PathBuf, VerifyReport)> = Vec::with_capacity(directories.len());
    for directory in directories {
        let verify_dir = directory.clone();
        let report = tokio::task::spawn_blocking(move || verify(&verify_dir, None))
            .await
            .map_err(|_| BackupError::new(ErrorKind::Io, "the verification task failed"))??;
        let mut entries = blob_entries(directory);
        let mut archived = false;
        while let Some(entry) = entries.next().await? {
            archived |= entry.archive_object_key.is_some();
        }
        let _ = entries.summary().ok_or_else(|| {
            BackupError::new(ErrorKind::Io, "the blob list verification was incomplete")
        })?;
        if archived {
            let access = archive.ok_or_else(|| {
                BackupError::new(
                    ErrorKind::SourceDamaged,
                    "archive backup inventory requires the configured provider and server key",
                )
            })?;
            verify_with_archive(directory, Some(&report.manifest_sha256), access).await?;
        }
        checked.push((directory.clone(), report));
    }

    let mut backups = 0_u64;
    let mut archive_references = 0_u64;
    for (directory, report) in checked {
        let Some(backup_id) = report
            .manifest
            .backup_id
            .as_deref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
        else {
            // Format v1 is local-only and has no durable archive identity.
            continue;
        };
        let refs =
            register_archive_pins(pool, &directory, backup_id, &report.manifest.blobs).await?;
        if !db::archive_backup_complete(pool, backup_id, &report.manifest_sha256, refs).await? {
            return Err(BackupError::new(
                ErrorKind::Integrity,
                "the imported archive pins do not match the backup manifest",
            ));
        }
        backups = backups.checked_add(1).ok_or_else(|| {
            BackupError::new(ErrorKind::Integrity, "backup inventory count overflow")
        })?;
        archive_references = archive_references.checked_add(refs).ok_or_else(|| {
            BackupError::new(
                ErrorKind::Integrity,
                "archive inventory reference count overflow",
            )
        })?;
    }

    if !db::archive_gc_mark_inventory_complete(pool, operator_confirmed_complete).await? {
        return Err(BackupError::new(
            ErrorKind::Busy,
            "archive inventory cannot be marked complete while pending pin registrations remain",
        ));
    }
    Ok(InventoryReport {
        backups,
        archive_references,
    })
}
