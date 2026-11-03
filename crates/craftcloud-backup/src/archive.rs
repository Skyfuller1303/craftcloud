//! Remote archive checks used before backups and restores.

use craftcloud_storage::{
    ArchiveCrypto, ArchiveStore, CHUNK_BYTES, ObjectKey, ObjectStore, ReadSource, StorageError,
    WrappedDataKey, encrypted_len,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    blob_stream::blob_entries,
    error::{BackupError, ErrorKind},
    manifest::{BlobEntry, ChunkEntry, parse_sha256},
    verify::BlobListSum,
};

/// Provider and server key required for archive-aware backup operations.
#[derive(Clone, Copy)]
pub struct ArchiveAccess<'a> {
    /// Configured archive provider.
    pub store: &'a ArchiveStore,
    /// Archive-only key wrapper derived from the server secret.
    pub crypto: &'a ArchiveCrypto,
    /// Maximum objects fully read and decrypted during one backup check.
    pub verify_sample: usize,
}

/// Checks every referenced provider object exists with the expected encrypted
/// size, then fully decrypts and hashes a deterministic random sample.
pub(crate) async fn verify_remote_refs(
    access: ArchiveAccess<'_>,
    backup_id: Uuid,
    dir: &std::path::Path,
) -> Result<BlobListSum, BackupError> {
    let mut sample = Vec::<(u64, BlobEntry)>::new();
    let mut entries = blob_entries(dir);
    while let Some(entry) = entries.next().await? {
        let Some(archive_key) = entry.archive_object_key.as_deref() else {
            continue;
        };
        let key = ObjectKey::parse(archive_key)
            .map_err(|_| BackupError::new(ErrorKind::Format, "an archive object key is invalid"))?;
        let ciphertext_len = encrypted_len(entry.size).map_err(|_| {
            BackupError::new(ErrorKind::Integrity, "archive ciphertext length overflow")
        })?;
        let head = match access.store.head(&key).await {
            Ok(head) => head,
            Err(StorageError::NotFound) => {
                return Err(BackupError::new(
                    ErrorKind::SourceDamaged,
                    format!("archive object {archive_key} is missing"),
                ));
            }
            Err(error) => return Err(error.into()),
        };
        if head.len != ciphertext_len {
            return Err(BackupError::new(
                ErrorKind::SourceDamaged,
                format!("archive object {archive_key} has an unexpected encrypted size"),
            ));
        }
        if access.verify_sample > 0 {
            let rank = sample_rank(backup_id, archive_key);
            if sample.len() < access.verify_sample {
                sample.push((rank, entry.clone()));
                sample.sort_by_key(|(rank, _)| *rank);
            } else if sample.last().is_some_and(|(last, _)| rank < *last) {
                if let Some(last) = sample.last_mut() {
                    *last = (rank, entry.clone());
                }
                sample.sort_by_key(|(rank, _)| *rank);
            }
        }
    }
    let sum = entries
        .summary()
        .ok_or_else(|| BackupError::new(ErrorKind::Io, "the blob list was not fully read"))?;
    let sum = BlobListSum {
        sha256: sum.sha256,
        count: sum.count,
        total_bytes: sum.total_bytes,
    };
    for (_, entry) in sample {
        verify_one(access.store, access.crypto, &entry).await?;
    }
    Ok(sum)
}

pub(crate) async fn verify_remote_chunks(
    access: ArchiveAccess<'_>,
    backup_id: Uuid,
    dir: &std::path::Path,
) -> Result<(), BackupError> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<ChunkEntry, BackupError>>(8);
    let path = dir.to_path_buf();
    let producer = tokio::task::spawn_blocking(move || {
        let result = crate::verify::for_each_chunk(&path, |entry| {
            sender
                .blocking_send(Ok(entry.clone()))
                .map_err(|_| BackupError::new(ErrorKind::Io, "chunk verification was cancelled"))
        });
        if let Err(error) = &result {
            let _ = sender.blocking_send(Err(error.clone()));
        }
        result
    });
    let mut sample = Vec::<(u64, ChunkEntry)>::new();
    let mut failure = None;
    while let Some(next) = receiver.recv().await {
        let entry = match next {
            Ok(entry) => entry,
            Err(error) => {
                failure = Some(error);
                break;
            }
        };
        let Some(archive_key) = entry.archive_object_key.as_deref() else {
            continue;
        };
        let key = ObjectKey::parse(archive_key)
            .map_err(|_| BackupError::new(ErrorKind::Format, "a chunk archive key is invalid"))?;
        let expected = encrypted_len(entry.stored_len()).map_err(|_| {
            BackupError::new(ErrorKind::Integrity, "chunk ciphertext length overflow")
        })?;
        let head = access.store.head(&key).await.map_err(|error| {
            if matches!(error, StorageError::NotFound) {
                BackupError::new(
                    ErrorKind::SourceDamaged,
                    format!("archive chunk {archive_key} is missing"),
                )
            } else {
                error.into()
            }
        })?;
        if head.len != expected {
            failure = Some(BackupError::new(
                ErrorKind::SourceDamaged,
                format!("archive chunk {archive_key} has an unexpected encrypted size"),
            ));
            break;
        }
        if access.verify_sample > 0 {
            let rank = sample_rank(backup_id, archive_key);
            if sample.len() < access.verify_sample {
                sample.push((rank, entry));
                sample.sort_by_key(|(rank, _)| *rank);
            } else if sample.last().is_some_and(|(last, _)| rank < *last) {
                if let Some(last) = sample.last_mut() {
                    *last = (rank, entry);
                }
                sample.sort_by_key(|(rank, _)| *rank);
            }
        }
    }
    drop(receiver);
    let produced = producer.await;
    if failure.is_none() {
        failure = match produced {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some(BackupError::new(ErrorKind::Io, "chunk verification failed")),
        };
    }
    if let Some(error) = failure {
        return Err(error);
    }
    for (_, entry) in sample {
        let stored = read_archive_chunk(access.store, access.crypto, &entry).await?;
        verify_chunk_payload(&entry, &stored)?;
    }
    Ok(())
}

pub(crate) fn verify_chunk_payload(
    entry: &ChunkEntry,
    stored: &[u8],
) -> Result<Vec<u8>, BackupError> {
    let digest = parse_sha256(&entry.sha256)
        .ok_or_else(|| BackupError::new(ErrorKind::Format, "a chunk checksum is invalid"))?;
    craftcloud_storage::compression::decode_chunk(&entry.encoding, stored, entry.size, &digest)
        .map_err(|_| BackupError::new(ErrorKind::Integrity, "a stored chunk is damaged"))
}

pub(crate) async fn read_archive_chunk(
    store: &ArchiveStore,
    crypto: &ArchiveCrypto,
    entry: &ChunkEntry,
) -> Result<Vec<u8>, BackupError> {
    let chunk_id = Uuid::parse_str(&entry.id)
        .map_err(|_| BackupError::new(ErrorKind::Format, "a chunk id is invalid"))?;
    let archive_key = entry
        .archive_object_key
        .as_deref()
        .ok_or_else(|| BackupError::new(ErrorKind::Format, "a chunk archive key is missing"))?;
    let key = ObjectKey::parse(archive_key)
        .map_err(|_| BackupError::new(ErrorKind::Format, "a chunk archive key is invalid"))?;
    let wrapped = entry.archive_wrapped_key.as_deref().ok_or_else(|| {
        BackupError::new(ErrorKind::Format, "a chunk archive envelope is missing")
    })?;
    let envelope = WrappedDataKey::from_bytes(&parse_hex(wrapped)?).map_err(|_| {
        BackupError::new(ErrorKind::Integrity, "a chunk archive envelope is invalid")
    })?;
    let data_key = crypto
        .unwrap_data_key(&envelope, chunk_id, entry.stored_len())
        .map_err(|_| {
            BackupError::new(
                ErrorKind::Integrity,
                "a chunk archive key cannot be unwrapped",
            )
        })?;
    let expected_len = encrypted_len(entry.stored_len())
        .map_err(|_| BackupError::new(ErrorKind::Integrity, "chunk ciphertext length overflow"))?;
    let source = store.open_read(&key, None).await.map_err(|error| {
        if matches!(error, StorageError::NotFound) {
            BackupError::new(
                ErrorKind::SourceDamaged,
                format!("archive chunk {archive_key} is missing"),
            )
        } else {
            error.into()
        }
    })?;
    if source.remaining() != expected_len {
        return Err(BackupError::new(
            ErrorKind::SourceDamaged,
            format!("archive chunk {archive_key} has an unexpected size"),
        ));
    }
    let mut exact = ExactReader::new(source);
    let mut plaintext = Vec::with_capacity(usize::try_from(entry.stored_len()).unwrap_or(0));
    let mut remaining = entry.stored_len();
    let mut index = 0_u64;
    loop {
        let plain_len = usize::try_from(remaining.min(CHUNK_BYTES as u64))
            .map_err(|_| BackupError::new(ErrorKind::Integrity, "archive block size overflow"))?;
        let cipher = exact.read_exact(plain_len + 16).await?;
        let plain = data_key
            .decrypt_block(&envelope, index, &cipher)
            .map_err(|_| {
                BackupError::new(
                    ErrorKind::Integrity,
                    "an archive chunk block failed authentication",
                )
            })?;
        plaintext.extend_from_slice(&plain);
        remaining -= plain_len as u64;
        if remaining == 0 {
            break;
        }
        index = index.checked_add(1).ok_or_else(|| {
            BackupError::new(ErrorKind::Integrity, "archive block index overflow")
        })?;
    }
    exact.finish().await?;
    Ok(plaintext)
}

fn sample_rank(backup_id: Uuid, key: &str) -> u64 {
    let mut digest = Sha256::new();
    digest.update(backup_id.as_bytes());
    digest.update(key.as_bytes());
    let hash = digest.finalize();
    let prefix = hash
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .unwrap_or([0; 8]);
    u64::from_be_bytes(prefix)
}

async fn verify_one(
    store: &ArchiveStore,
    crypto: &ArchiveCrypto,
    entry: &BlobEntry,
) -> Result<(), BackupError> {
    let blob_id = Uuid::parse_str(&entry.id)
        .map_err(|_| BackupError::new(ErrorKind::Format, "a blob id is invalid"))?;
    let archive_key = entry
        .archive_object_key
        .as_deref()
        .ok_or_else(|| BackupError::new(ErrorKind::Format, "an archive key is missing"))?;
    let key = ObjectKey::parse(archive_key)
        .map_err(|_| BackupError::new(ErrorKind::Format, "an archive object key is invalid"))?;
    let wrapped = entry
        .archive_wrapped_key
        .as_deref()
        .ok_or_else(|| BackupError::new(ErrorKind::Format, "an archive envelope is missing"))?;
    let envelope_bytes = parse_hex(wrapped)?;
    let envelope = WrappedDataKey::from_bytes(&envelope_bytes).map_err(|_| {
        BackupError::new(ErrorKind::Integrity, "an archive key envelope is invalid")
    })?;
    let expected_digest = parse_sha256(&entry.sha256)
        .ok_or_else(|| BackupError::new(ErrorKind::Format, "a blob checksum is invalid"))?;
    let data_key = crypto
        .unwrap_data_key(&envelope, blob_id, entry.size)
        .map_err(|_| {
            BackupError::new(ErrorKind::Integrity, "an archive key cannot be unwrapped")
        })?;
    let expected_len = encrypted_len(entry.size).map_err(|_| {
        BackupError::new(ErrorKind::Integrity, "archive ciphertext length overflow")
    })?;
    let source = store
        .open_read(&key, None)
        .await
        .map_err(|error| match error {
            StorageError::NotFound => BackupError::new(
                ErrorKind::SourceDamaged,
                format!("archive object {archive_key} is missing"),
            ),
            other => other.into(),
        })?;
    if source.remaining() != expected_len {
        return Err(BackupError::new(
            ErrorKind::SourceDamaged,
            format!("archive object {archive_key} has an unexpected encrypted size"),
        ));
    }
    let mut exact = ExactReader::new(source);
    let mut digest = Sha256::new();
    let mut remaining = entry.size;
    let mut index = 0_u64;
    loop {
        let plain_len = usize::try_from(remaining.min(CHUNK_BYTES as u64))
            .map_err(|_| BackupError::new(ErrorKind::Integrity, "archive block size overflow"))?;
        let cipher = exact.read_exact(plain_len + 16).await?;
        let plain = data_key
            .decrypt_block(&envelope, index, &cipher)
            .map_err(|_| {
                BackupError::new(
                    ErrorKind::Integrity,
                    "an archive block failed authentication",
                )
            })?;
        digest.update(&plain);
        remaining -= plain_len as u64;
        if remaining == 0 {
            break;
        }
        index = index.checked_add(1).ok_or_else(|| {
            BackupError::new(ErrorKind::Integrity, "archive block index overflow")
        })?;
    }
    exact.finish().await?;
    if <[u8; 32]>::from(digest.finalize()) != expected_digest {
        return Err(BackupError::new(
            ErrorKind::Integrity,
            format!("archive object {archive_key} does not match its plaintext checksum"),
        ));
    }
    Ok(())
}

fn parse_hex(text: &str) -> Result<Vec<u8>, BackupError> {
    if !text.len().is_multiple_of(2)
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(BackupError::new(
            ErrorKind::Format,
            "an archive envelope is not lowercase hex",
        ));
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for [high_byte, low_byte] in text.as_bytes().as_chunks::<2>().0 {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let high = digit(*high_byte)
            .ok_or_else(|| BackupError::new(ErrorKind::Format, "an archive envelope is invalid"))?;
        let low = digit(*low_byte)
            .ok_or_else(|| BackupError::new(ErrorKind::Format, "an archive envelope is invalid"))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

struct ExactReader<R> {
    source: R,
    pending: Vec<u8>,
    offset: usize,
}

impl<R: ReadSource> ExactReader<R> {
    fn new(source: R) -> Self {
        Self {
            source,
            pending: Vec::new(),
            offset: 0,
        }
    }

    async fn read_exact(&mut self, count: usize) -> Result<Vec<u8>, BackupError> {
        let mut output = Vec::with_capacity(count);
        while output.len() < count {
            if self.offset == self.pending.len() {
                self.pending = self.source.next_chunk().await?.ok_or_else(|| {
                    BackupError::new(ErrorKind::SourceDamaged, "archive object ended early")
                })?;
                self.offset = 0;
                if self.pending.is_empty() || self.pending.len() > CHUNK_BYTES {
                    return Err(BackupError::new(
                        ErrorKind::SourceDamaged,
                        "archive provider returned an invalid chunk",
                    ));
                }
            }
            let take = (count - output.len()).min(self.pending.len() - self.offset);
            let end = self.offset.checked_add(take).ok_or_else(|| {
                BackupError::new(ErrorKind::Integrity, "archive chunk offset overflow")
            })?;
            let bytes = self.pending.get(self.offset..end).ok_or_else(|| {
                BackupError::new(
                    ErrorKind::SourceDamaged,
                    "archive provider returned an invalid chunk",
                )
            })?;
            output.extend_from_slice(bytes);
            self.offset += take;
        }
        Ok(output)
    }

    async fn finish(&mut self) -> Result<(), BackupError> {
        if self.offset != self.pending.len() || self.source.next_chunk().await?.is_some() {
            return Err(BackupError::new(
                ErrorKind::SourceDamaged,
                "archive object has trailing bytes",
            ));
        }
        Ok(())
    }
}
