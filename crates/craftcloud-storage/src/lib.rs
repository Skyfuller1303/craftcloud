//! CraftCloud object storage (CC-F09): the [`ObjectStore`] trait and the local
//! filesystem adapter [`LocalStore`].
//!
//! Design rules:
//! - **Keys are server generated.** An [`ObjectKey`] is a random UUID v4 in
//!   lowercase hex. No client string, tenant id or file name ever becomes part
//!   of a path, and there is no deduplication (identical contents get distinct
//!   keys, so storage reveals nothing about other tenants' data).
//! - **Nothing partial is ever visible.** Data is streamed into a temporary
//!   object, verified (length and SHA-256) and only then published by an atomic
//!   rename. A failed, aborted or dropped writer leaves no object behind.
//! - **Bounded memory.** Writes use a 64 KiB buffer, reads deliver chunks of at
//!   most 64 KiB, and every write has a caller supplied byte limit.
//! - **Typed errors without paths.** [`StorageError`] never contains file
//!   system paths; paths only appear in server logs.
//!
//! This crate has no HTTP code. Uploads and downloads use it from the API
//! crate in later milestones.

mod error;
mod faults;
mod key;
mod local;

pub use error::{StorageError, StoreSetupError, map_io_error};
#[cfg(any(test, feature = "fault-injection"))]
pub use faults::{FaultKind, FaultPlan};
pub use key::{KeyError, ObjectKey};
pub use local::{LocalStore, ObjectReader, TempObject};

use std::time::SystemTime;

/// A SHA-256 digest.
pub type Sha256Digest = [u8; 32];

/// Size of the write buffer and the largest chunk a reader delivers.
pub const CHUNK_BYTES: usize = 64 * 1024;

/// Length of a stored object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectHead {
    /// Size in bytes.
    pub len: u64,
}

/// A published object found by [`ObjectStore::list_objects`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    /// The key.
    pub key: ObjectKey,
    /// Size in bytes.
    pub len: u64,
    /// Last modification time, if the file system reports one.
    pub modified: Option<SystemTime>,
}

/// An unfinished temporary object found by [`ObjectStore::list_temp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempInfo {
    /// Identifier of the temporary object (32 lowercase hex characters).
    pub id: String,
    /// Bytes on disk so far.
    pub len: u64,
    /// Last modification time, if the file system reports one.
    pub modified: Option<SystemTime>,
}

/// A half-open byte range `start..end` of an object; `end: None` means up to
/// the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// First byte.
    pub start: u64,
    /// One past the last byte; clamped to the object length.
    pub end: Option<u64>,
}

/// Where a streamed write goes.
#[allow(async_fn_in_trait)]
pub trait WriteSink {
    /// Appends `data`. Fails with [`StorageError::TooLarge`] when the byte
    /// limit would be exceeded; after any error the writer is dead and its
    /// temporary data is already removed.
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError>;

    /// Bytes accepted so far.
    fn bytes_written(&self) -> u64;
}

/// A bounded stream of an object's bytes.
#[allow(async_fn_in_trait)]
pub trait ReadSource {
    /// The next chunk (at most [`CHUNK_BYTES`]), or `None` at the end.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, StorageError>;

    /// Bytes this stream will still deliver.
    fn remaining(&self) -> u64;
}

/// Object storage. Implementations never overwrite a published object and
/// never expose partial data.
#[allow(async_fn_in_trait)]
pub trait ObjectStore {
    /// Writer type returned by [`ObjectStore::begin_write`].
    type Temp: WriteSink;
    /// Reader type returned by [`ObjectStore::open_read`].
    type Reader: ReadSource;

    /// Starts a streamed write of at most `max_bytes` bytes.
    async fn begin_write(&self, max_bytes: u64) -> Result<Self::Temp, StorageError>;

    /// Verifies length and SHA-256 and publishes the object under a new key.
    /// On any failure the temporary object is discarded.
    async fn finalize(
        &self,
        temp: Self::Temp,
        expected_len: u64,
        expected_sha256: &Sha256Digest,
    ) -> Result<ObjectKey, StorageError>;

    /// Discards a temporary object.
    async fn abort(&self, temp: Self::Temp);

    /// Streams an object, optionally only a byte range.
    async fn open_read(
        &self,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<Self::Reader, StorageError>;

    /// Size of an object.
    async fn head(&self, key: &ObjectKey) -> Result<ObjectHead, StorageError>;

    /// Deletes an object. The caller decides whether it is still referenced.
    async fn delete(&self, key: &ObjectKey) -> Result<(), StorageError>;

    /// Re-reads an object and compares its SHA-256.
    async fn verify(
        &self,
        key: &ObjectKey,
        expected_sha256: &Sha256Digest,
    ) -> Result<(), StorageError>;

    /// Published objects ordered by key, at most `limit`, after `after`.
    async fn list_objects(
        &self,
        after: Option<&ObjectKey>,
        limit: usize,
    ) -> Result<Vec<ObjectInfo>, StorageError>;

    /// Temporary objects ordered by id, at most `limit`, after `after`.
    async fn list_temp(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TempInfo>, StorageError>;

    /// Removes a temporary object by id (reconciliation of crashed uploads).
    async fn remove_temp(&self, id: &str) -> Result<(), StorageError>;
}
