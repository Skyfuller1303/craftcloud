//! Fault injection for tests: fail a write at byte N, at fsync or at publish,
//! or simulate a full device. Arming a fault is only possible with the
//! `fault-injection` feature (or in this crate's own tests); the server never
//! enables it. A default plan injects nothing and costs a few atomic loads.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::error::StorageError;

/// What an injected failure looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// A generic I/O error.
    Io,
    /// "No space left on device".
    Full,
}

impl FaultKind {
    pub(crate) fn error(self) -> StorageError {
        match self {
            Self::Io => StorageError::Io(io::Error::other("injected fault")),
            Self::Full => StorageError::Full,
        }
    }
}

/// Injected failures. Share one plan (in an `Arc`) with the store under test
/// and arm it from the test; every fault fires once and disarms itself.
#[derive(Debug)]
pub struct FaultPlan {
    fail_write_at: AtomicU64,
    write_kind_full: AtomicBool,
    fail_fsync: AtomicBool,
    fail_rename: AtomicBool,
}

const NEVER: u64 = u64::MAX;

impl Default for FaultPlan {
    fn default() -> Self {
        Self {
            fail_write_at: AtomicU64::new(NEVER),
            write_kind_full: AtomicBool::new(false),
            fail_fsync: AtomicBool::new(false),
            fail_rename: AtomicBool::new(false),
        }
    }
}

impl FaultPlan {
    /// Fails the disk write that reaches byte offset `offset` (the bytes before it
    /// are written to disk first, like a device that fills up midway). Writes
    /// are buffered in 64 KiB blocks, so the failure surfaces from the write
    /// that flushes the block, or from `finalize`.
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn fail_write_at(&self, offset: u64, kind: FaultKind) {
        self.write_kind_full
            .store(kind == FaultKind::Full, Ordering::SeqCst);
        self.fail_write_at.store(offset, Ordering::SeqCst);
    }

    /// Fails the next fsync of a data file.
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn fail_fsync(&self) {
        self.fail_fsync.store(true, Ordering::SeqCst);
    }

    /// Fails the next publishing step (the hard link).
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn fail_rename(&self) {
        self.fail_rename.store(true, Ordering::SeqCst);
    }

    /// The offset at which writes fail, if armed.
    pub(crate) fn write_limit(&self) -> Option<u64> {
        let at = self.fail_write_at.load(Ordering::SeqCst);
        (at != NEVER).then_some(at)
    }

    /// Consumes the armed write fault.
    pub(crate) fn take_write_fault(&self) -> StorageError {
        self.fail_write_at.store(NEVER, Ordering::SeqCst);
        if self.write_kind_full.load(Ordering::SeqCst) {
            FaultKind::Full.error()
        } else {
            FaultKind::Io.error()
        }
    }

    pub(crate) fn take_fsync_fault(&self) -> Option<StorageError> {
        self.fail_fsync
            .swap(false, Ordering::SeqCst)
            .then(|| FaultKind::Io.error())
    }

    pub(crate) fn take_rename_fault(&self) -> Option<StorageError> {
        self.fail_rename
            .swap(false, Ordering::SeqCst)
            .then(|| FaultKind::Io.error())
    }
}
