//! Bounded encrypted archive upload and end-to-end verification.
//! Local bytes are never deleted by this module.

use craftcloud_storage::{
    ArchiveCrypto, ArchiveCryptoError, ArchiveDataKey, ArchiveRange, ByteRange, CHUNK_BYTES,
    ObjectKey, ObjectStore, ReadSource, StorageError, WrappedDataKey, WriteSink, encrypted_len,
    map_range, verify_ciphertext_len,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// An archive copy that has been published, read back and verified.
pub struct VerifiedArchive {
    /// Opaque provider object key.
    pub object_key: ObjectKey,
    /// Wrapped data key and authenticated format metadata.
    pub envelope: WrappedDataKey,
}

/// Archive transport failures. No error contains file content or key material.
#[derive(Debug)]
pub enum ArchiveTransportError {
    /// A store operation failed.
    Storage(StorageError),
    /// Authenticated encryption or metadata validation failed.
    Crypto(ArchiveCryptoError),
    /// Data length or content did not match the database record.
    Integrity,
}

impl From<StorageError> for ArchiveTransportError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}
impl From<ArchiveCryptoError> for ArchiveTransportError {
    fn from(value: ArchiveCryptoError) -> Self {
        Self::Crypto(value)
    }
}

/// Bounded exact reads from a store whose chunk boundaries can vary.
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
    async fn exact(&mut self, count: usize) -> Result<Vec<u8>, ArchiveTransportError> {
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            if self.offset == self.pending.len() {
                self.pending = self
                    .source
                    .next_chunk()
                    .await?
                    .ok_or(ArchiveTransportError::Integrity)?;
                self.offset = 0;
                if self.pending.is_empty() || self.pending.len() > CHUNK_BYTES {
                    return Err(ArchiveTransportError::Integrity);
                }
            }
            let take = (count - out.len()).min(self.pending.len() - self.offset);
            let end = self
                .offset
                .checked_add(take)
                .ok_or(ArchiveTransportError::Integrity)?;
            let chunk = self
                .pending
                .get(self.offset..end)
                .ok_or(ArchiveTransportError::Integrity)?;
            out.extend_from_slice(chunk);
            self.offset += take;
        }
        Ok(out)
    }
    async fn end(&mut self) -> Result<(), ArchiveTransportError> {
        if self.offset != self.pending.len() || self.source.next_chunk().await?.is_some() {
            return Err(ArchiveTransportError::Integrity);
        }
        Ok(())
    }
}

/// Encrypts a local blob to an immutable archive object, then reads the entire
/// published ciphertext back and verifies every authenticated block and the
/// plaintext SHA-256. A failure never deletes the local source.
pub async fn upload_verified<S: ObjectStore>(
    local: &craftcloud_storage::LocalStore,
    archive: &S,
    crypto: &ArchiveCrypto,
    blob_id: Uuid,
    local_key: &ObjectKey,
    size: u64,
    expected_sha256: &[u8; 32],
) -> Result<VerifiedArchive, ArchiveTransportError> {
    let source = local.open_read(local_key, None).await?;
    if source.remaining() != size {
        return Err(ArchiveTransportError::Integrity);
    }
    let (envelope, data_key) = crypto.create_data_key(blob_id, size)?;
    let cipher_len = encrypted_len(size)?;
    let mut temp = archive.begin_write(cipher_len).await?;
    let mut source = ExactReader::new(source);
    let mut cipher_hash = Sha256::new();
    let mut plain_hash = Sha256::new();
    let mut remaining = size;
    let mut index = 0_u64;
    loop {
        let length = usize::try_from(remaining.min(CHUNK_BYTES as u64))
            .map_err(|_| ArchiveTransportError::Integrity)?;
        let result = async {
            let bytes = source.exact(length).await?;
            plain_hash.update(&bytes);
            let encrypted = data_key.encrypt_block(&envelope, index, &bytes)?;
            cipher_hash.update(&encrypted);
            temp.write(&encrypted).await?;
            Ok::<(), ArchiveTransportError>(())
        }
        .await;
        if let Err(error) = result {
            archive.abort(temp).await;
            return Err(error);
        }
        remaining -= length as u64;
        if remaining == 0 {
            break;
        }
        index = index
            .checked_add(1)
            .ok_or(ArchiveTransportError::Integrity)?;
    }
    if source.end().await.is_err() || <[u8; 32]>::from(plain_hash.finalize()) != *expected_sha256 {
        archive.abort(temp).await;
        return Err(ArchiveTransportError::Integrity);
    }
    let cipher_sha = cipher_hash.finalize().into();
    let key = archive.finalize(temp, cipher_len, &cipher_sha).await?;
    let verify = verify_archive(
        archive,
        crypto,
        &key,
        &envelope,
        blob_id,
        size,
        expected_sha256,
    )
    .await;
    // Readback may fail after publication. Keep the immutable object so a
    // retry can verify and reuse it; never turn an uncertain publish into
    // an orphan or delete a key another worker may already reference.
    verify?;
    Ok(VerifiedArchive {
        object_key: key,
        envelope,
    })
}

/// Publishes a previously persisted archive intent, or verifies its existing
/// immutable object. The full local digest is checked before unwrapping and
/// reusing the intent key; the encrypted streaming pass checks it again before
/// any ciphertext is published.
// Keep this call shape until archive_worker is edited in its owned follow-up.
#[allow(clippy::too_many_arguments)]
pub async fn upload_intent_verified<S: ObjectStore>(
    local: &craftcloud_storage::LocalStore,
    archive: &S,
    crypto: &ArchiveCrypto,
    blob_id: Uuid,
    local_key: &ObjectKey,
    archive_key: &ObjectKey,
    envelope: &WrappedDataKey,
    size: u64,
    expected_sha256: &[u8; 32],
) -> Result<VerifiedArchive, ArchiveTransportError> {
    match archive.head(archive_key).await {
        Ok(_) => {
            verify_archive(
                archive,
                crypto,
                archive_key,
                envelope,
                blob_id,
                size,
                expected_sha256,
            )
            .await?;
            return Ok(VerifiedArchive {
                object_key: archive_key.clone(),
                envelope: envelope.clone(),
            });
        }
        Err(StorageError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }

    // Verify before reusing the persisted data key and nonce prefix. A second
    // digest during encryption protects against mutation after this pass.
    let local_head = local.head(local_key).await?;
    if local_head.len != size {
        return Err(ArchiveTransportError::Integrity);
    }
    match local.verify(local_key, expected_sha256).await {
        Ok(()) => {}
        Err(StorageError::ChecksumMismatch | StorageError::LengthMismatch) => {
            return Err(ArchiveTransportError::Integrity);
        }
        Err(error) => return Err(error.into()),
    }

    let data_key = crypto.unwrap_data_key(envelope, blob_id, size)?;
    let cipher_len = encrypted_len(size)?;
    let mut temp = match archive
        .begin_write_as(archive_key.clone(), cipher_len)
        .await
    {
        Ok(temp) => temp,
        Err(StorageError::AlreadyExists) => {
            match verify_archive(
                archive,
                crypto,
                archive_key,
                envelope,
                blob_id,
                size,
                expected_sha256,
            )
            .await
            {
                Ok(()) => {
                    return Ok(VerifiedArchive {
                        object_key: archive_key.clone(),
                        envelope: envelope.clone(),
                    });
                }
                Err(ArchiveTransportError::Storage(StorageError::NotFound)) => {}
                Err(error) => return Err(error),
            }
            remove_stale_temp(archive, archive_key).await?;
            archive
                .begin_write_as(archive_key.clone(), cipher_len)
                .await?
        }
        Err(error) => return Err(error.into()),
    };
    if !matches!(temp.object_key(), Ok(ref key) if key == archive_key) {
        archive.abort(temp).await;
        return Err(ArchiveTransportError::Integrity);
    }

    let source = local.open_read(local_key, None).await?;
    if source.remaining() != size {
        archive.abort(temp).await;
        return Err(ArchiveTransportError::Integrity);
    }
    let mut source = ExactReader::new(source);
    let mut cipher_hash = Sha256::new();
    let mut plain_hash = Sha256::new();
    let mut remaining = size;
    let mut index = 0_u64;
    loop {
        let length = usize::try_from(remaining.min(CHUNK_BYTES as u64))
            .map_err(|_| ArchiveTransportError::Integrity)?;
        let result = async {
            let bytes = source.exact(length).await?;
            plain_hash.update(&bytes);
            let encrypted = data_key.encrypt_block(envelope, index, &bytes)?;
            cipher_hash.update(&encrypted);
            temp.write(&encrypted).await?;
            Ok::<(), ArchiveTransportError>(())
        }
        .await;
        if let Err(error) = result {
            archive.abort(temp).await;
            return Err(error);
        }
        remaining -= length as u64;
        if remaining == 0 {
            break;
        }
        index = index
            .checked_add(1)
            .ok_or(ArchiveTransportError::Integrity)?;
    }
    if source.end().await.is_err() || <[u8; 32]>::from(plain_hash.finalize()) != *expected_sha256 {
        archive.abort(temp).await;
        return Err(ArchiveTransportError::Integrity);
    }
    let cipher_sha = cipher_hash.finalize().into();
    match archive.finalize(temp, cipher_len, &cipher_sha).await {
        Ok(published) if published == *archive_key => {}
        Ok(_) => return Err(ArchiveTransportError::Integrity),
        Err(publish_error) => {
            match verify_archive(
                archive,
                crypto,
                archive_key,
                envelope,
                blob_id,
                size,
                expected_sha256,
            )
            .await
            {
                Ok(()) => {}
                Err(ArchiveTransportError::Storage(StorageError::NotFound)) => {
                    remove_stale_temp(archive, archive_key).await?;
                    return Err(publish_error.into());
                }
                Err(error) => return Err(error),
            }
        }
    }
    verify_archive(
        archive,
        crypto,
        archive_key,
        envelope,
        blob_id,
        size,
        expected_sha256,
    )
    .await?;
    Ok(VerifiedArchive {
        object_key: archive_key.clone(),
        envelope: envelope.clone(),
    })
}

async fn remove_stale_temp<S: ObjectStore>(
    archive: &S,
    archive_key: &ObjectKey,
) -> Result<(), ArchiveTransportError> {
    match archive.remove_temp(archive_key.as_str()).await {
        Ok(()) | Err(StorageError::NotFound) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Reads and authenticates every block and checks length and plaintext digest.
pub async fn verify_archive<S: ObjectStore>(
    archive: &S,
    crypto: &ArchiveCrypto,
    object_key: &ObjectKey,
    envelope: &WrappedDataKey,
    blob_id: Uuid,
    size: u64,
    expected_sha256: &[u8; 32],
) -> Result<(), ArchiveTransportError> {
    let data_key = crypto.unwrap_data_key(envelope, blob_id, size)?;
    verify_ciphertext_len(size, archive.head(object_key).await?.len)?;
    let reader = archive.open_read(object_key, None).await?;
    verify_ciphertext_len(size, reader.remaining())?;
    let mut source = ExactReader::new(reader);
    let mut digest = Sha256::new();
    let mut remaining = size;
    let mut index = 0_u64;
    loop {
        let plain_length = usize::try_from(remaining.min(CHUNK_BYTES as u64))
            .map_err(|_| ArchiveTransportError::Integrity)?;
        let ciphertext = source.exact(plain_length + 16).await?;
        let plain = data_key.decrypt_block(envelope, index, &ciphertext)?;
        digest.update(&plain);
        remaining -= plain_length as u64;
        if remaining == 0 {
            break;
        }
        index = index
            .checked_add(1)
            .ok_or(ArchiveTransportError::Integrity)?;
    }
    source.end().await?;
    if <[u8; 32]>::from(digest.finalize()) != *expected_sha256 {
        return Err(ArchiveTransportError::Integrity);
    }
    Ok(())
}

/// A bounded reader of authenticated plaintext blocks from an archive object.
pub struct ArchiveReader<R> {
    source: ExactReader<R>,
    data_key: ArchiveDataKey,
    envelope: WrappedDataKey,
    range: ArchiveRange,
    next_block: u64,
    remaining: u64,
}

impl<R: ReadSource> ArchiveReader<R> {
    /// Number of plaintext bytes remaining in the requested range.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Returns the next authenticated range chunk, never exceeding 64 KiB.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ArchiveTransportError> {
        if self.next_block >= self.range.end_block {
            return Ok(None);
        }
        let block = self.next_block;
        let block_start = block
            .checked_mul(CHUNK_BYTES as u64)
            .ok_or(ArchiveTransportError::Integrity)?;
        let block_len = usize::try_from(
            self.envelope
                .plaintext_len()
                .saturating_sub(block_start)
                .min(CHUNK_BYTES as u64),
        )
        .map_err(|_| ArchiveTransportError::Integrity)?;
        let ciphertext = self.source.exact(block_len + 16).await?;
        let plaintext = self
            .data_key
            .decrypt_block(&self.envelope, block, &ciphertext)?;
        self.next_block += 1;
        if self.next_block == self.range.end_block {
            self.source.end().await?;
        }
        let start = usize::try_from(
            self.range
                .plaintext_start
                .saturating_sub(block_start)
                .min(block_len as u64),
        )
        .map_err(|_| ArchiveTransportError::Integrity)?;
        let end = usize::try_from(
            self.range
                .plaintext_end
                .saturating_sub(block_start)
                .min(block_len as u64),
        )
        .map_err(|_| ArchiveTransportError::Integrity)?;
        if end < start {
            return Err(ArchiveTransportError::Integrity);
        }
        let result = plaintext
            .get(start..end)
            .ok_or(ArchiveTransportError::Integrity)?
            .to_vec();
        self.remaining = self
            .remaining
            .checked_sub(result.len() as u64)
            .ok_or(ArchiveTransportError::Integrity)?;
        if result.is_empty() {
            return Ok(None);
        }
        Ok(Some(result))
    }
}

/// Opens a plaintext byte range. Checks the full remote length before fetching
/// the authenticated blocks and validates the envelope against the blob ID.
pub async fn open_archive_range<S: ObjectStore>(
    archive: &S,
    crypto: &ArchiveCrypto,
    object_key: &ObjectKey,
    envelope: &WrappedDataKey,
    blob_id: Uuid,
    size: u64,
    range: Option<ByteRange>,
) -> Result<ArchiveReader<S::Reader>, ArchiveTransportError> {
    let data_key = crypto.unwrap_data_key(envelope, blob_id, size)?;
    verify_ciphertext_len(size, archive.head(object_key).await?.len)?;
    let mapped = map_range(size, range)?;
    let source = archive
        .open_read(object_key, Some(mapped.ciphertext))
        .await?;
    let expected = mapped
        .ciphertext
        .end
        .ok_or(ArchiveTransportError::Integrity)?
        .checked_sub(mapped.ciphertext.start)
        .ok_or(ArchiveTransportError::Integrity)?;
    if source.remaining() != expected {
        return Err(ArchiveTransportError::Integrity);
    }
    let mut reader = ArchiveReader {
        source: ExactReader::new(source),
        data_key,
        envelope: envelope.clone(),
        range: mapped,
        next_block: mapped.first_block,
        remaining: mapped.plaintext_end - mapped.plaintext_start,
    };
    // Empty objects still carry an authenticated block; check it before success.
    if size == 0 {
        let _ = reader.next_chunk().await?;
    }
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use craftcloud_storage::LocalStore;

    #[tokio::test]
    async fn archive_upload_is_encrypted_verified_and_source_survives_errors() {
        let root = std::env::temp_dir().join(format!("craftcloud-ac12-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("local")).unwrap();
        std::fs::create_dir_all(root.join("remote")).unwrap();
        let local = LocalStore::open(&root.join("local")).await.unwrap();
        let remote = LocalStore::open(&root.join("remote")).await.unwrap();
        let crypto = ArchiveCrypto::from_server_key(&[7; 32]).unwrap();
        for size in [0, CHUNK_BYTES + 123] {
            let bytes = vec![b'x'; size];
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            let mut tmp = local.begin_write(size as u64).await.unwrap();
            tmp.write(&bytes).await.unwrap();
            let key = local.finalize(tmp, size as u64, &digest).await.unwrap();
            let blob_id = Uuid::new_v4();
            let copy = upload_verified(
                &local,
                &remote,
                &crypto,
                blob_id,
                &key,
                size as u64,
                &digest,
            )
            .await
            .unwrap();
            verify_archive(
                &remote,
                &crypto,
                &copy.object_key,
                &copy.envelope,
                blob_id,
                size as u64,
                &digest,
            )
            .await
            .unwrap();
            if size > CHUNK_BYTES {
                let start = CHUNK_BYTES as u64 - 20;
                let end = CHUNK_BYTES as u64 + 40;
                let mut reader = open_archive_range(
                    &remote,
                    &crypto,
                    &copy.object_key,
                    &copy.envelope,
                    blob_id,
                    size as u64,
                    Some(ByteRange {
                        start,
                        end: Some(end),
                    }),
                )
                .await
                .unwrap();
                let mut range = Vec::new();
                while let Some(chunk) = reader.next_chunk().await.unwrap() {
                    range.extend_from_slice(&chunk);
                }
                assert_eq!(range, bytes[start as usize..end as usize]);
                assert_eq!(reader.remaining(), 0);
            } else {
                let mut reader = open_archive_range(
                    &remote,
                    &crypto,
                    &copy.object_key,
                    &copy.envelope,
                    blob_id,
                    size as u64,
                    None,
                )
                .await
                .unwrap();
                assert!(reader.next_chunk().await.unwrap().is_none());
            }
            let mut raw = remote.open_read(&copy.object_key, None).await.unwrap();
            let mut encrypted = Vec::new();
            while let Some(chunk) = raw.next_chunk().await.unwrap() {
                encrypted.extend_from_slice(&chunk);
            }
            assert_ne!(encrypted, bytes);
            if size > 0 {
                assert!(!encrypted.windows(64).any(|window| window == &bytes[..64]));
            }
            assert!(
                upload_verified(
                    &local,
                    &remote,
                    &crypto,
                    blob_id,
                    &key,
                    size as u64,
                    &[0; 32]
                )
                .await
                .is_err()
            );
            assert_eq!(local.head(&key).await.unwrap().len, size as u64);
        }
        drop(local);
        drop(remote);
        std::fs::remove_dir_all(root).unwrap();
    }
}
