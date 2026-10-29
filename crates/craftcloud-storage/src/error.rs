//! Typed storage errors. `Display` never contains file system paths.

use std::fmt;
use std::io;

/// Why a storage operation failed.
#[derive(Debug)]
pub enum StorageError {
    /// The device is full (ENOSPC, `ERROR_DISK_FULL`). Maps to
    /// 507 `INSUFFICIENT_STORAGE` at the API edge.
    Full,
    /// The write would exceed the byte limit given to `begin_write`.
    TooLarge,
    /// The SHA-256 of the data differs from the expected one.
    ChecksumMismatch,
    /// The number of bytes differs from the expected length.
    LengthMismatch,
    /// No such object (or temporary object).
    NotFound,
    /// The requested range starts after the end of the object.
    RangeNotSatisfiable,
    /// A published object with this key already exists (never overwritten).
    AlreadyExists,
    /// The writer failed or was aborted earlier and cannot be used any more.
    Aborted,
    /// Any other I/O failure. The inner error may contain a path; log it on
    /// the server, never send it to clients.
    Io(io::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full => "storage is full",
            Self::TooLarge => "data exceeds the size limit",
            Self::ChecksumMismatch => "checksum mismatch",
            Self::LengthMismatch => "length mismatch",
            Self::NotFound => "object not found",
            Self::RangeNotSatisfiable => "range not satisfiable",
            Self::AlreadyExists => "object already exists",
            Self::Aborted => "write was aborted",
            Self::Io(_) => "storage I/O error",
        })
    }
}

impl std::error::Error for StorageError {}

impl StorageError {
    /// Whether the error means "no space left".
    #[must_use]
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Raw OS codes for "disk full": ENOSPC on unix, `ERROR_HANDLE_DISK_FULL`
/// (39) and `ERROR_DISK_FULL` (112) on Windows.
#[cfg(unix)]
const DISK_FULL_CODES: &[i32] = &[28];
#[cfg(not(unix))]
const DISK_FULL_CODES: &[i32] = &[39, 112];

/// Maps an I/O error to a typed one: disk-full conditions become
/// [`StorageError::Full`], missing files [`StorageError::NotFound`].
#[must_use]
pub fn map_io_error(error: io::Error) -> StorageError {
    if error.kind() == io::ErrorKind::StorageFull {
        return StorageError::Full;
    }
    if error.kind() == io::ErrorKind::NotFound {
        return StorageError::NotFound;
    }
    if error
        .raw_os_error()
        .is_some_and(|code| DISK_FULL_CODES.contains(&code))
    {
        return StorageError::Full;
    }
    StorageError::Io(error)
}

/// Why the store could not be opened. The message names the problem (and the
/// configured path, which the operator knows); it is meant for the server log
/// at startup.
#[derive(Debug)]
pub struct StoreSetupError(pub(crate) String);

impl fmt::Display for StoreSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreSetupError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_full_conditions_map_to_full() {
        assert!(matches!(
            map_io_error(io::Error::from(io::ErrorKind::StorageFull)),
            StorageError::Full
        ));
        #[cfg(unix)]
        assert!(matches!(
            map_io_error(io::Error::from_raw_os_error(28)),
            StorageError::Full
        ));
        #[cfg(windows)]
        for code in [39, 112] {
            assert!(matches!(
                map_io_error(io::Error::from_raw_os_error(code)),
                StorageError::Full
            ));
        }
    }

    #[test]
    fn other_errors_keep_their_class_and_display_has_no_paths() {
        assert!(matches!(
            map_io_error(io::Error::from(io::ErrorKind::NotFound)),
            StorageError::NotFound
        ));
        let other = map_io_error(io::Error::other("C:\\secret\\path\\x"));
        assert!(matches!(other, StorageError::Io(_)));
        assert!(!other.to_string().contains("secret"));
        assert!(!StorageError::Full.to_string().contains('/'));
    }
}
