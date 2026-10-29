//! Errors of the backup tooling. The message is for the operator on the
//! console: it names the problem and, where useful, a backup-relative file or
//! an object key, but never a connection string, a password or file content.

use std::fmt;

/// What went wrong, for callers and tests that must tell the cases apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A file or directory could not be read or written.
    Io,
    /// The database failed.
    Database,
    /// The object store failed.
    Storage,
    /// The database schema is not what this build expects (backup) or what the
    /// backup expects (restore).
    Schema,
    /// The maintenance lock is held by someone else (maintenance run or
    /// another backup) and did not become free in time.
    Busy,
    /// The database and the store directory do not belong together.
    Binding,
    /// The manifest cannot be read or has an unknown format or migration version.
    Format,
    /// A checksum, size or file list does not match the manifest.
    Integrity,
    /// The restore target (database or store) already holds data.
    TargetNotEmpty,
    /// An object of the source store is missing or does not match its
    /// recorded checksum, so no complete backup can be made.
    SourceDamaged,
}

/// A backup, verify or restore failure.
#[derive(Debug, Clone)]
pub struct BackupError {
    /// The category.
    pub kind: ErrorKind,
    /// A sentence for the operator.
    pub message: String,
}

impl BackupError {
    /// A new error.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn io(what: &str, error: &std::io::Error) -> Self {
        Self::new(ErrorKind::Io, format!("{what}: {error}"))
    }
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BackupError {}

impl From<craftcloud_db::DbError> for BackupError {
    fn from(error: craftcloud_db::DbError) -> Self {
        Self::new(ErrorKind::Database, error.to_string())
    }
}

impl From<sqlx::Error> for BackupError {
    fn from(error: sqlx::Error) -> Self {
        // Through DbError so that driver text never reaches the console.
        Self::from(craftcloud_db::DbError::from(error))
    }
}

impl From<craftcloud_storage::StorageError> for BackupError {
    fn from(error: craftcloud_storage::StorageError) -> Self {
        Self::new(ErrorKind::Storage, error.to_string())
    }
}
