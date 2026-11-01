//! Consistent backup, offline verification and restore of a CraftCloud
//! instance (CC-F14): the database tables exported from one snapshot, the
//! referenced blobs, and a manifest with SHA-256 checksums.
//!
//! The backup needs no `pg_dump` (the runtime image has no shell and no
//! PostgreSQL client): tables travel as `COPY` text through the normal
//! database connection. See [`create`], [`verify`] and [`restore`].
//!
//! Backups contain everything a database administrator can read: password
//! hashes, e-mail addresses, the audit log and all file contents. They are
//! written readable by their owner only on unix (mode 0700/0600; on other
//! platforms the permissions of the target directory apply). Encryption at rest and the off-site
//! copy are the operator's decision (docs/06-operations/backup-upgrade-runbook.md).

mod archive;
mod blob_stream;
mod create;
mod error;
mod inventory;
pub mod manifest;
mod restore;
pub mod tables;
mod verify;

pub use archive::ArchiveAccess;
pub use create::{CreateOptions, CreateReport, Phase, PhaseHook, create, create_with_archive};
pub use error::{BackupError, ErrorKind};
pub use inventory::{InventoryReport, inventory_backups, release_backup_pins};
pub use manifest::Manifest;
pub use restore::{
    RestoreHook, RestoreOptions, RestorePhase, RestoreReport, restore, restore_with_archive,
};
pub use verify::{
    VerifyReport, preflight_restore_archive, read_manifest, verify, verify_with_archive,
};
