//! Versioned encryption primitives for opaque archive objects.
//!
//! Each object gets a fresh data-encryption key and nonce prefix. A wrapped
//! key envelope binds that key to a stable blob UUID and plaintext length.
//! Ciphertext blocks use XChaCha20-Poly1305 with the UUID, total length, block
//! index and block length as associated data.

use std::fmt;

use chacha20poly1305::{AeadInPlace, KeyInit, Tag, XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{ByteRange, CHUNK_BYTES};

const FORMAT: &[u8; 8] = b"CCARCV01";
const ENVELOPE_BYTES: usize = 8 + 16 + 8 + 16 + 24 + 48;
const TAG_BYTES: usize = 16;
const KEY_BYTES: usize = 32;

type HmacSha256 = Hmac<Sha256>;

/// Wrapped per-object key metadata. `Debug` never exposes key bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedDataKey {
    blob_id: Uuid,
    plaintext_len: u64,
    nonce_prefix: [u8; 16],
    wrap_nonce: [u8; 24],
    wrapped_key: [u8; 48],
}

impl fmt::Debug for WrappedDataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrappedDataKey")
            .field("format", &1)
            .field("blob_id", &self.blob_id)
            .field("plaintext_len", &self.plaintext_len)
            .field("key_material", &"[REDACTED]")
            .finish()
    }
}

impl WrappedDataKey {
    /// Stable blob identity associated with every encrypted block.
    #[must_use]
    pub const fn blob_id(&self) -> Uuid {
        self.blob_id
    }

    /// Plaintext byte length bound into the key envelope and every block.
    #[must_use]
    pub const fn plaintext_len(&self) -> u64 {
        self.plaintext_len
    }

    /// Serializes versioned, fixed-size key metadata for database storage.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; ENVELOPE_BYTES] {
        let mut out = [0; ENVELOPE_BYTES];
        out[..8].copy_from_slice(FORMAT);
        out[8..24].copy_from_slice(self.blob_id.as_bytes());
        out[24..32].copy_from_slice(&self.plaintext_len.to_le_bytes());
        out[32..48].copy_from_slice(&self.nonce_prefix);
        out[48..72].copy_from_slice(&self.wrap_nonce);
        out[72..].copy_from_slice(&self.wrapped_key);
        out
    }

    /// Parses only the supported format and exact envelope size.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ArchiveCryptoError> {
        if bytes.len() != ENVELOPE_BYTES || bytes.get(..8) != Some(FORMAT.as_slice()) {
            return Err(ArchiveCryptoError::InvalidEnvelope);
        }
        let blob_id = Uuid::from_slice(
            bytes
                .get(8..24)
                .ok_or(ArchiveCryptoError::InvalidEnvelope)?,
        )
        .map_err(|_| ArchiveCryptoError::InvalidEnvelope)?;
        let plaintext_len = u64::from_le_bytes(
            bytes
                .get(24..32)
                .ok_or(ArchiveCryptoError::InvalidEnvelope)?
                .try_into()
                .map_err(|_| ArchiveCryptoError::InvalidEnvelope)?,
        );
        let mut nonce_prefix = [0; 16];
        nonce_prefix.copy_from_slice(
            bytes
                .get(32..48)
                .ok_or(ArchiveCryptoError::InvalidEnvelope)?,
        );
        let mut wrap_nonce = [0; 24];
        wrap_nonce.copy_from_slice(
            bytes
                .get(48..72)
                .ok_or(ArchiveCryptoError::InvalidEnvelope)?,
        );
        let mut wrapped_key = [0; 48];
        wrapped_key.copy_from_slice(bytes.get(72..).ok_or(ArchiveCryptoError::InvalidEnvelope)?);
        Ok(Self {
            blob_id,
            plaintext_len,
            nonce_prefix,
            wrap_nonce,
            wrapped_key,
        })
    }
}

/// Archive encryption failure. Display strings contain no secret material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveCryptoError {
    /// Envelope bytes are malformed or use an unsupported format.
    InvalidEnvelope,
    /// Key unwrap or block authentication failed.
    Authentication,
    /// Block index or length disagrees with the envelope.
    InvalidBlock,
    /// Remote object length differs from its expected ciphertext length.
    CiphertextLength,
    /// Range or length arithmetic overflowed or exceeded the object.
    InvalidRange,
    /// Archive key derivation failed.
    KeyDerivation,
}

impl fmt::Display for ArchiveCryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidEnvelope => "invalid archive key envelope",
            Self::Authentication => "archive authentication failed",
            Self::InvalidBlock => "invalid archive block",
            Self::CiphertextLength => "archive ciphertext length mismatch",
            Self::InvalidRange => "invalid archive range",
            Self::KeyDerivation => "archive key derivation failed",
        })
    }
}

impl std::error::Error for ArchiveCryptoError {}

/// Server-side key wrapper. Construct only from the configured server key.
pub struct ArchiveCrypto {
    wrapping_key: [u8; KEY_BYTES],
}

impl fmt::Debug for ArchiveCrypto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveCrypto")
            .field("wrapping_key", &"[REDACTED]")
            .finish()
    }
}

impl Drop for ArchiveCrypto {
    fn drop(&mut self) {
        self.wrapping_key.zeroize();
    }
}

impl ArchiveCrypto {
    /// Derives the archive-only wrapping key from the 32-byte server key.
    /// Context is fixed to `craftcloud/archive/v1` and distinct from other uses.
    pub fn from_server_key(server_key: &[u8; KEY_BYTES]) -> Result<Self, ArchiveCryptoError> {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(server_key)
            .map_err(|_| ArchiveCryptoError::KeyDerivation)?;
        mac.update(b"craftcloud/archive/v1");
        let mut wrapping_key = [0; KEY_BYTES];
        wrapping_key.copy_from_slice(&mac.finalize().into_bytes());
        Ok(Self { wrapping_key })
    }

    /// Generates a fresh object key, nonce prefix and wrapped envelope.
    pub fn create_data_key(
        &self,
        blob_id: Uuid,
        plaintext_len: u64,
    ) -> Result<(WrappedDataKey, ArchiveDataKey), ArchiveCryptoError> {
        let mut key = [0; KEY_BYTES];
        let key_uuid = Uuid::new_v4();
        let key_uuid2 = Uuid::new_v4();
        key[..16].copy_from_slice(key_uuid.as_bytes());
        key[16..].copy_from_slice(key_uuid2.as_bytes());
        let data_key = ArchiveDataKey(key);
        key.zeroize();
        let nonce_prefix = *Uuid::new_v4().as_bytes();
        let wrap_nonce = random_nonce();
        let cipher = XChaCha20Poly1305::new((&self.wrapping_key).into());
        let mut wrapped_key = [0; 48];
        wrapped_key[..32].copy_from_slice(&data_key.0);
        let aad = envelope_aad(blob_id, plaintext_len);
        let tag = cipher
            .encrypt_in_place_detached(
                XNonce::from_slice(&wrap_nonce),
                &aad,
                &mut wrapped_key[..32],
            )
            .map_err(|_| ArchiveCryptoError::Authentication)?;
        wrapped_key[32..].copy_from_slice(&tag);
        let envelope = WrappedDataKey {
            blob_id,
            plaintext_len,
            nonce_prefix,
            wrap_nonce,
            wrapped_key,
        };
        Ok((envelope, data_key))
    }

    /// Unwraps an envelope after checking blob identity and declared length.
    pub fn unwrap_data_key(
        &self,
        envelope: &WrappedDataKey,
        blob_id: Uuid,
        plaintext_len: u64,
    ) -> Result<ArchiveDataKey, ArchiveCryptoError> {
        if envelope.blob_id != blob_id || envelope.plaintext_len != plaintext_len {
            return Err(ArchiveCryptoError::Authentication);
        }
        let cipher = XChaCha20Poly1305::new((&self.wrapping_key).into());
        let mut key = [0; KEY_BYTES];
        key.copy_from_slice(&envelope.wrapped_key[..KEY_BYTES]);
        let tag = Tag::clone_from_slice(&envelope.wrapped_key[KEY_BYTES..]);
        let aad = envelope_aad(blob_id, plaintext_len);
        cipher
            .decrypt_in_place_detached(
                XNonce::from_slice(&envelope.wrap_nonce),
                &aad,
                &mut key,
                &tag,
            )
            .map_err(|_| {
                key.zeroize();
                ArchiveCryptoError::Authentication
            })?;
        Ok(ArchiveDataKey(key))
    }
}

/// Per-object data key. Debug and Drop protect its raw bytes.
pub struct ArchiveDataKey([u8; KEY_BYTES]);

impl fmt::Debug for ArchiveDataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArchiveDataKey([REDACTED])")
    }
}

impl Drop for ArchiveDataKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl ArchiveDataKey {
    /// Encrypts one plaintext block. Caller supplies exact block index.
    pub fn encrypt_block(
        &self,
        envelope: &WrappedDataKey,
        index: u64,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ArchiveCryptoError> {
        validate_block(envelope, index, plaintext.len())?;
        let cipher = XChaCha20Poly1305::new((&self.0).into());
        let nonce = block_nonce(&envelope.nonce_prefix, index);
        let aad = block_aad(envelope, index, plaintext.len())?;
        let mut out = plaintext.to_vec();
        cipher
            .encrypt_in_place(XNonce::from_slice(&nonce), &aad, &mut out)
            .map_err(|_| ArchiveCryptoError::Authentication)?;
        Ok(out)
    }

    /// Decrypts and authenticates exactly one block.
    pub fn decrypt_block(
        &self,
        envelope: &WrappedDataKey,
        index: u64,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, ArchiveCryptoError> {
        let plain_len = ciphertext
            .len()
            .checked_sub(TAG_BYTES)
            .ok_or(ArchiveCryptoError::InvalidBlock)?;
        validate_block(envelope, index, plain_len)?;
        let cipher = XChaCha20Poly1305::new((&self.0).into());
        let nonce = block_nonce(&envelope.nonce_prefix, index);
        let aad = block_aad(envelope, index, plain_len)?;
        let mut out = ciphertext.to_vec();
        cipher
            .decrypt_in_place(XNonce::from_slice(&nonce), &aad, &mut out)
            .map_err(|_| {
                out.zeroize();
                ArchiveCryptoError::Authentication
            })?;
        Ok(out)
    }
}

/// A plaintext range and the aligned encrypted byte range needed to serve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveRange {
    /// Ciphertext bytes to fetch from the remote store.
    pub ciphertext: ByteRange,
    /// First encrypted block index included.
    pub first_block: u64,
    /// One past the final encrypted block index.
    pub end_block: u64,
    /// First requested plaintext byte, relative to the object.
    pub plaintext_start: u64,
    /// One past the final requested plaintext byte.
    pub plaintext_end: u64,
}

/// Expected remote object size, including authentication tags.
pub fn encrypted_len(plaintext_len: u64) -> Result<u64, ArchiveCryptoError> {
    let blocks = block_count(plaintext_len)?;
    plaintext_len
        .checked_add(
            blocks
                .checked_mul(TAG_BYTES as u64)
                .ok_or(ArchiveCryptoError::InvalidRange)?,
        )
        .ok_or(ArchiveCryptoError::InvalidRange)
}

/// Verifies remote length before any range read, detecting truncation anywhere.
pub fn verify_ciphertext_len(plaintext_len: u64, actual: u64) -> Result<(), ArchiveCryptoError> {
    if encrypted_len(plaintext_len)? == actual {
        Ok(())
    } else {
        Err(ArchiveCryptoError::CiphertextLength)
    }
}

/// Maps a plaintext range to the complete authenticated ciphertext blocks.
pub fn map_range(
    plaintext_len: u64,
    range: Option<ByteRange>,
) -> Result<ArchiveRange, ArchiveCryptoError> {
    let (start, end) = match range {
        Some(r) => (r.start, r.end.unwrap_or(plaintext_len).min(plaintext_len)),
        None => (0, plaintext_len),
    };
    if start > plaintext_len || end < start {
        return Err(ArchiveCryptoError::InvalidRange);
    }
    let block_size = u64::try_from(CHUNK_BYTES).map_err(|_| ArchiveCryptoError::InvalidRange)?;
    let empty_object = plaintext_len == 0;
    let first = if empty_object || start == end {
        0
    } else {
        start / block_size
    };
    let end_block = if empty_object {
        1
    } else if start == end {
        0
    } else {
        end.checked_add(block_size - 1)
            .ok_or(ArchiveCryptoError::InvalidRange)?
            / block_size
    };
    let cipher_start = first
        .checked_mul(block_size + TAG_BYTES as u64)
        .ok_or(ArchiveCryptoError::InvalidRange)?;
    let cipher_end = if end_block == 0 {
        0
    } else {
        let prior = end_block
            .checked_sub(1)
            .ok_or(ArchiveCryptoError::InvalidRange)?;
        let plain_end = prior
            .checked_add(1)
            .and_then(|n| n.checked_mul(block_size))
            .ok_or(ArchiveCryptoError::InvalidRange)?
            .min(plaintext_len);
        plain_end
            .checked_add(
                end_block
                    .checked_mul(TAG_BYTES as u64)
                    .ok_or(ArchiveCryptoError::InvalidRange)?,
            )
            .ok_or(ArchiveCryptoError::InvalidRange)?
    };
    Ok(ArchiveRange {
        ciphertext: ByteRange {
            start: cipher_start,
            end: Some(cipher_end),
        },
        first_block: first,
        end_block,
        plaintext_start: start,
        plaintext_end: end,
    })
}

fn block_count(len: u64) -> Result<u64, ArchiveCryptoError> {
    if len == 0 {
        return Ok(1);
    }
    let size = u64::try_from(CHUNK_BYTES).map_err(|_| ArchiveCryptoError::InvalidRange)?;
    len.checked_add(size - 1)
        .ok_or(ArchiveCryptoError::InvalidRange)
        .map(|v| v / size)
}

fn expected_block_len(total: u64, index: u64) -> Result<usize, ArchiveCryptoError> {
    if total == 0 {
        return if index == 0 {
            Ok(0)
        } else {
            Err(ArchiveCryptoError::InvalidBlock)
        };
    }
    let size = u64::try_from(CHUNK_BYTES).map_err(|_| ArchiveCryptoError::InvalidRange)?;
    let start = index
        .checked_mul(size)
        .ok_or(ArchiveCryptoError::InvalidRange)?;
    if start >= total {
        return Err(ArchiveCryptoError::InvalidBlock);
    }
    usize::try_from((total - start).min(size)).map_err(|_| ArchiveCryptoError::InvalidBlock)
}

fn validate_block(
    envelope: &WrappedDataKey,
    index: u64,
    len: usize,
) -> Result<(), ArchiveCryptoError> {
    if expected_block_len(envelope.plaintext_len, index)? == len {
        Ok(())
    } else {
        Err(ArchiveCryptoError::InvalidBlock)
    }
}

fn envelope_aad(id: Uuid, len: u64) -> [u8; 32] {
    let mut aad = [0; 32];
    aad[..8].copy_from_slice(FORMAT);
    aad[8..24].copy_from_slice(id.as_bytes());
    aad[24..].copy_from_slice(&len.to_le_bytes());
    aad
}

fn block_aad(
    envelope: &WrappedDataKey,
    index: u64,
    len: usize,
) -> Result<[u8; 44], ArchiveCryptoError> {
    let mut aad = [0; 44];
    aad[..8].copy_from_slice(FORMAT);
    aad[8..24].copy_from_slice(envelope.blob_id.as_bytes());
    aad[24..32].copy_from_slice(&envelope.plaintext_len.to_le_bytes());
    aad[32..40].copy_from_slice(&index.to_le_bytes());
    aad[40..].copy_from_slice(
        &u32::try_from(len)
            .map_err(|_| ArchiveCryptoError::InvalidBlock)?
            .to_le_bytes(),
    );
    Ok(aad)
}

fn block_nonce(prefix: &[u8; 16], index: u64) -> [u8; 24] {
    let mut nonce = [0; 24];
    nonce[..16].copy_from_slice(prefix);
    nonce[16..].copy_from_slice(&index.to_le_bytes());
    nonce
}

fn random_nonce() -> [u8; 24] {
    let mut nonce = [0; 24];
    nonce[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    nonce[16..].copy_from_slice(&Uuid::new_v4().as_bytes()[..8]);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crypto(key: &[u8; 32]) -> Result<ArchiveCrypto, ArchiveCryptoError> {
        ArchiveCrypto::from_server_key(key)
    }

    #[test]
    fn key_envelope_and_blocks_round_trip_and_bind_identity_and_length()
    -> Result<(), ArchiveCryptoError> {
        let wrapper = crypto(&[7; 32])?;
        let id = Uuid::new_v4();
        let data = vec![42; CHUNK_BYTES + 31];
        let (envelope, data_key) = wrapper.create_data_key(id, data.len() as u64)?;
        let encoded = envelope.to_bytes();
        let decoded = WrappedDataKey::from_bytes(&encoded)?;
        let key = wrapper.unwrap_data_key(&decoded, id, data.len() as u64)?;
        let wrong_wrapper = crypto(&[6; 32])?;
        assert_eq!(
            wrong_wrapper
                .unwrap_data_key(&decoded, id, data.len() as u64)
                .err(),
            Some(ArchiveCryptoError::Authentication)
        );
        let first = key.encrypt_block(&decoded, 0, &data[..CHUNK_BYTES])?;
        let last = key.encrypt_block(&decoded, 1, &data[CHUNK_BYTES..])?;
        assert_eq!(
            data_key.decrypt_block(&decoded, 0, &first)?,
            data[..CHUNK_BYTES]
        );
        assert_eq!(key.decrypt_block(&decoded, 1, &last)?, data[CHUNK_BYTES..]);
        assert_eq!(
            wrapper
                .unwrap_data_key(&decoded, Uuid::new_v4(), data.len() as u64)
                .err(),
            Some(ArchiveCryptoError::Authentication)
        );
        assert_eq!(
            wrapper
                .unwrap_data_key(&decoded, id, data.len() as u64 + 1)
                .err(),
            Some(ArchiveCryptoError::Authentication)
        );
        assert_eq!(
            WrappedDataKey::from_bytes(&encoded[..encoded.len() - 1]).err(),
            Some(ArchiveCryptoError::InvalidEnvelope)
        );
        Ok(())
    }

    #[test]
    fn reordered_truncated_and_tampered_blocks_fail_authentication()
    -> Result<(), ArchiveCryptoError> {
        let wrapper = crypto(&[8; 32])?;
        let id = Uuid::new_v4();
        let (envelope, key) = wrapper.create_data_key(id, (CHUNK_BYTES * 2) as u64)?;
        let plain = vec![9; CHUNK_BYTES];
        let first = key.encrypt_block(&envelope, 0, &plain)?;
        assert_eq!(
            key.decrypt_block(&envelope, 1, &first).err(),
            Some(ArchiveCryptoError::Authentication)
        );
        assert_eq!(
            key.decrypt_block(&envelope, 0, &first[..first.len() - 1])
                .err(),
            Some(ArchiveCryptoError::InvalidBlock)
        );
        let mut changed = first.clone();
        if let Some(byte) = changed.first_mut() {
            *byte ^= 1;
        }
        assert_eq!(
            key.decrypt_block(&envelope, 0, &changed).err(),
            Some(ArchiveCryptoError::Authentication)
        );
        Ok(())
    }

    #[test]
    fn ranges_cover_full_blocks_and_validate_remote_length() -> Result<(), ArchiveCryptoError> {
        let range = map_range(
            (CHUNK_BYTES * 2 + 3) as u64,
            Some(ByteRange {
                start: (CHUNK_BYTES - 2) as u64,
                end: Some((CHUNK_BYTES + 1) as u64),
            }),
        )?;
        assert_eq!(range.first_block, 0);
        assert_eq!(range.end_block, 2);
        assert_eq!(range.ciphertext.start, 0);
        assert_eq!(range.ciphertext.end, Some((CHUNK_BYTES * 2 + 32) as u64));
        assert_eq!(encrypted_len(0), Ok(TAG_BYTES as u64));
        assert_eq!(verify_ciphertext_len(0, TAG_BYTES as u64), Ok(()));
        assert_eq!(
            verify_ciphertext_len(0, 0),
            Err(ArchiveCryptoError::CiphertextLength)
        );
        let empty = map_range(0, None)?;
        assert_eq!(
            empty.ciphertext,
            ByteRange {
                start: 0,
                end: Some(TAG_BYTES as u64)
            }
        );
        let empty_id = Uuid::new_v4();
        let (empty_envelope, empty_key) = crypto(&[4; 32])?.create_data_key(empty_id, 0)?;
        let empty_ciphertext = empty_key.encrypt_block(&empty_envelope, 0, &[])?;
        assert_eq!(empty_ciphertext.len(), TAG_BYTES);
        assert!(
            empty_key
                .decrypt_block(&empty_envelope, 0, &empty_ciphertext)?
                .is_empty()
        );
        Ok(())
    }
}
