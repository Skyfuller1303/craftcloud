//! Provider-selecting storage adapter for archive runtime configuration.

use crate::{
    ByteRange, LocalStore, ObjectHead, ObjectInfo, ObjectKey, ObjectStore, ReadSource, S3Reader,
    S3Store, S3Temp, SftpReader, SftpStore, Sha256Digest, StorageError, TempInfo, TempObject,
    WriteSink,
};

/// Configured archive provider. Runtime selects one concrete adapter at startup.
pub enum ArchiveStore {
    /// S3-compatible object storage.
    S3(S3Store),
    /// Pinned-host-key SFTP storage.
    Sftp(SftpStore),
    /// Local filesystem storage for tests and development.
    Local(LocalStore),
}

/// Reader returned by [`ArchiveStore`] regardless of its provider.
pub enum ArchiveReader {
    /// S3 stream.
    S3(S3Reader),
    /// SFTP stream.
    Sftp(Box<SftpReader>),
    /// Local file reader.
    Local(crate::ObjectReader),
}

/// Writer returned by [`ArchiveStore`] regardless of its provider.
pub enum ArchiveTemp {
    /// S3 local staging writer.
    S3(S3Temp),
    /// SFTP local staging writer.
    Sftp(TempObject),
    /// Local filesystem writer.
    Local(TempObject),
}

impl ArchiveStore {
    /// Applies one decimal-Mbit/s budget to provider transfer and readback bytes.
    pub fn set_bandwidth_limit_mbps(&mut self, mbps: Option<u64>) {
        match self {
            Self::S3(store) => store.set_bandwidth_limit_mbps(mbps),
            Self::Sftp(store) => store.set_bandwidth_limit_mbps(mbps),
            Self::Local(_) => {}
        }
    }
}

#[allow(async_fn_in_trait)]
impl WriteSink for ArchiveTemp {
    fn object_key(&self) -> Result<ObjectKey, StorageError> {
        match self {
            Self::S3(temp) => temp.object_key(),
            Self::Sftp(temp) => temp.object_key(),
            Self::Local(temp) => temp.object_key(),
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        match self {
            Self::S3(temp) => temp.write(data).await,
            Self::Sftp(temp) => temp.write(data).await,
            Self::Local(temp) => temp.write(data).await,
        }
    }

    fn bytes_written(&self) -> u64 {
        match self {
            Self::S3(temp) => temp.bytes_written(),
            Self::Sftp(temp) => temp.bytes_written(),
            Self::Local(temp) => temp.bytes_written(),
        }
    }
}

#[allow(async_fn_in_trait)]
impl ReadSource for ArchiveReader {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, StorageError> {
        match self {
            Self::S3(reader) => reader.next_chunk().await,
            Self::Sftp(reader) => reader.next_chunk().await,
            Self::Local(reader) => reader.next_chunk().await,
        }
    }

    fn remaining(&self) -> u64 {
        match self {
            Self::S3(reader) => reader.remaining(),
            Self::Sftp(reader) => reader.remaining(),
            Self::Local(reader) => reader.remaining(),
        }
    }
}

#[allow(async_fn_in_trait)]
impl ObjectStore for ArchiveStore {
    type Temp = ArchiveTemp;
    type Reader = ArchiveReader;

    async fn begin_write(&self, max_bytes: u64) -> Result<Self::Temp, StorageError> {
        match self {
            Self::S3(store) => store.begin_write(max_bytes).await.map(ArchiveTemp::S3),
            Self::Sftp(store) => store.begin_write(max_bytes).await.map(ArchiveTemp::Sftp),
            Self::Local(store) => store.begin_write(max_bytes).await.map(ArchiveTemp::Local),
        }
    }

    async fn begin_write_as(
        &self,
        key: ObjectKey,
        max_bytes: u64,
    ) -> Result<Self::Temp, StorageError> {
        match self {
            Self::S3(store) => store
                .begin_write_as(key, max_bytes)
                .await
                .map(ArchiveTemp::S3),
            Self::Sftp(store) => store
                .begin_write_as(key, max_bytes)
                .await
                .map(ArchiveTemp::Sftp),
            Self::Local(store) => store
                .begin_write_as(key, max_bytes)
                .await
                .map(ArchiveTemp::Local),
        }
    }

    async fn finalize(
        &self,
        temp: Self::Temp,
        expected_len: u64,
        expected_sha256: &Sha256Digest,
    ) -> Result<ObjectKey, StorageError> {
        match (self, temp) {
            (Self::S3(store), ArchiveTemp::S3(temp)) => {
                store.finalize(temp, expected_len, expected_sha256).await
            }
            (Self::Sftp(store), ArchiveTemp::Sftp(temp)) => {
                store.finalize(temp, expected_len, expected_sha256).await
            }
            (Self::Local(store), ArchiveTemp::Local(temp)) => {
                store.finalize(temp, expected_len, expected_sha256).await
            }
            _ => Err(StorageError::Aborted),
        }
    }

    async fn abort(&self, temp: Self::Temp) {
        match (self, temp) {
            (Self::S3(store), ArchiveTemp::S3(temp)) => store.abort(temp).await,
            (Self::Sftp(store), ArchiveTemp::Sftp(temp)) => store.abort(temp).await,
            (Self::Local(store), ArchiveTemp::Local(temp)) => store.abort(temp).await,
            (_, ArchiveTemp::S3(temp)) => drop(temp),
            (_, ArchiveTemp::Sftp(temp) | ArchiveTemp::Local(temp)) => drop(temp),
        }
    }

    async fn open_read(
        &self,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<Self::Reader, StorageError> {
        match self {
            Self::S3(store) => store.open_read(key, range).await.map(ArchiveReader::S3),
            Self::Sftp(store) => store
                .open_read(key, range)
                .await
                .map(|reader| ArchiveReader::Sftp(Box::new(reader))),
            Self::Local(store) => store.open_read(key, range).await.map(ArchiveReader::Local),
        }
    }

    async fn head(&self, key: &ObjectKey) -> Result<ObjectHead, StorageError> {
        match self {
            Self::S3(store) => store.head(key).await,
            Self::Sftp(store) => store.head(key).await,
            Self::Local(store) => store.head(key).await,
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), StorageError> {
        match self {
            Self::S3(store) => store.delete(key).await,
            Self::Sftp(store) => store.delete(key).await,
            Self::Local(store) => store.delete(key).await,
        }
    }

    async fn verify(
        &self,
        key: &ObjectKey,
        expected_sha256: &Sha256Digest,
    ) -> Result<(), StorageError> {
        match self {
            Self::S3(store) => store.verify(key, expected_sha256).await,
            Self::Sftp(store) => store.verify(key, expected_sha256).await,
            Self::Local(store) => store.verify(key, expected_sha256).await,
        }
    }

    async fn list_objects(
        &self,
        after: Option<&ObjectKey>,
        limit: usize,
    ) -> Result<Vec<ObjectInfo>, StorageError> {
        match self {
            Self::S3(store) => store.list_objects(after, limit).await,
            Self::Sftp(store) => store.list_objects(after, limit).await,
            Self::Local(store) => store.list_objects(after, limit).await,
        }
    }

    async fn list_temp(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TempInfo>, StorageError> {
        match self {
            Self::S3(store) => store.list_temp(after, limit).await,
            Self::Sftp(store) => store.list_temp(after, limit).await,
            Self::Local(store) => store.list_temp(after, limit).await,
        }
    }

    async fn remove_temp(&self, id: &str) -> Result<(), StorageError> {
        match self {
            Self::S3(store) => store.remove_temp(id).await,
            Self::Sftp(store) => store.remove_temp(id).await,
            Self::Local(store) => store.remove_temp(id).await,
        }
    }
}
