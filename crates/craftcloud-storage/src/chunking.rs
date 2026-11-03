//! Stable, file-scoped FastCDC identities for immutable version chunks.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// AC-15 format version 1 lower chunk bound.
pub const MIN_CHUNK_BYTES: u32 = 256 * 1024;
/// AC-15 format version 1 target chunk size.
pub const TARGET_CHUNK_BYTES: u32 = 1024 * 1024;
/// AC-15 format version 1 upper chunk bound.
pub const MAX_CHUNK_BYTES: u32 = 4 * 1024 * 1024;

/// A chunk span in the input buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// First input byte.
    pub offset: usize,
    /// Number of bytes.
    pub length: usize,
}

/// A FastCDC input or parameter error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkError;

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid FastCDC input")
    }
}

impl std::error::Error for ChunkError {}

/// Splits a byte slice with fixed FastCDC v2020 parameters.
///
/// Empty input produces no chunks. Non-empty inputs produce bounded chunks.
pub fn fastcdc_chunks(input: &[u8]) -> Result<Vec<Chunk>, ChunkError> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let detector =
        fastcdc::v2020::FastCDC::new(input, MIN_CHUNK_BYTES, TARGET_CHUNK_BYTES, MAX_CHUNK_BYTES);
    detector
        .map(|chunk| {
            if chunk.length == 0
                || chunk
                    .offset
                    .checked_add(chunk.length)
                    .is_none_or(|end| end > input.len())
            {
                Err(ChunkError)
            } else {
                Ok(Chunk {
                    offset: chunk.offset,
                    length: chunk.length,
                })
            }
        })
        .collect()
}

/// Returns an HMAC identifier that cannot match across file boundaries.
pub fn chunk_digest(server_key: &[u8; 32], file_id: Uuid, content: &[u8]) -> Option<[u8; 32]> {
    let digest: [u8; 32] = sha2::Sha256::digest(content).into();
    chunk_digest_from_sha256(server_key, file_id, &digest)
}

/// Computes the canonical file-scoped identifier from a client's plaintext SHA-256.
pub fn chunk_digest_from_sha256(
    server_key: &[u8; 32],
    file_id: Uuid,
    plain_sha256: &[u8; 32],
) -> Option<[u8; 32]> {
    let mut mac = Hmac::<Sha256>::new_from_slice(server_key).ok()?;
    mac.update(b"craftcloud/chunk-identity/v1\0");
    mac.update(file_id.as_bytes());
    mac.update(plain_sha256);
    Some(mac.finalize().into_bytes().into())
}

/// Derives the version-1 chunk identity key from the instance server key.
pub fn derive_chunk_key(server_key: &[u8; 32]) -> Option<[u8; 32]> {
    let mut mac = Hmac::<Sha256>::new_from_slice(server_key).ok()?;
    mac.update(b"craftcloud/chunk-key/v1\0");
    Some(mac.finalize().into_bytes().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_handles_empty_and_one_byte_inputs() {
        assert!(fastcdc_chunks(&[]).unwrap_or_default().is_empty());
        assert_eq!(
            fastcdc_chunks(&[7]).unwrap_or_default(),
            vec![Chunk {
                offset: 0,
                length: 1
            }]
        );
    }

    #[test]
    fn chunking_is_deterministic_and_bounded() {
        let bytes = vec![42; 9 * TARGET_CHUNK_BYTES as usize];
        let first = fastcdc_chunks(&bytes).unwrap_or_default();
        assert_eq!(first, fastcdc_chunks(&bytes).unwrap_or_default());
        assert!(
            first
                .iter()
                .all(|chunk| chunk.length <= MAX_CHUNK_BYTES as usize)
        );
        assert!(
            first
                .iter()
                .take(first.len().saturating_sub(1))
                .all(|chunk| chunk.length >= MIN_CHUNK_BYTES as usize)
        );
        assert_eq!(
            first.iter().map(|chunk| chunk.length).sum::<usize>(),
            bytes.len()
        );
    }

    #[test]
    fn fastcdc_v1_boundary_fixture_is_stable() {
        let mut state = 0x51f0_a7c3_u32;
        let bytes: Vec<u8> = (0..8 * 1024 * 1024)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let boundaries: Vec<usize> = fastcdc_chunks(&bytes)
            .unwrap()
            .into_iter()
            .map(|chunk| chunk.length)
            .collect();
        assert_eq!(
            boundaries,
            vec![1_529_997, 1_708_052, 1_761_647, 893_944, 1_566_347, 928_621]
        );
    }

    #[test]
    fn small_insertions_keep_most_chunk_content() {
        let mut state = 7u32;
        let base: Vec<u8> = (0..24 * TARGET_CHUNK_BYTES as usize)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let original = chunk_contents(&base);
        for offset in [0, base.len() / 2, base.len()] {
            let mut changed = base.clone();
            changed.splice(offset..offset, [0xa5; 64]);
            let next = chunk_contents(&changed);
            let original_bytes: usize = original.iter().map(Vec::len).sum();
            let reused_bytes: usize = next
                .iter()
                .filter(|chunk| original.contains(chunk))
                .map(Vec::len)
                .sum();
            assert!(
                reused_bytes * 5 >= original_bytes * 4,
                "offset={offset}, reused={reused_bytes}, total={original_bytes}"
            );
        }
    }

    fn chunk_contents(input: &[u8]) -> Vec<Vec<u8>> {
        fastcdc_chunks(input)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|chunk| {
                input
                    .get(chunk.offset..chunk.offset + chunk.length)
                    .map(<[u8]>::to_vec)
            })
            .collect()
    }

    #[test]
    fn chunk_identity_is_file_scoped() {
        let key = [9; 32];
        let bytes = b"same bytes";
        assert_ne!(
            chunk_digest(&key, Uuid::nil(), bytes),
            chunk_digest(&key, Uuid::new_v4(), bytes)
        );
        assert_eq!(
            chunk_digest(&key, Uuid::nil(), bytes),
            chunk_digest(&key, Uuid::nil(), bytes)
        );
    }

    #[test]
    fn chunk_identifier_from_plaintext_hash_is_stable() {
        let key = [9; 32];
        let file_id = Uuid::from_u128(1);
        let plain_sha: [u8; 32] = sha2::Sha256::digest(b"same bytes").into();
        assert_eq!(
            chunk_digest_from_sha256(&key, file_id, &plain_sha),
            chunk_digest(&key, file_id, b"same bytes"),
        );
        assert_eq!(
            chunk_digest_from_sha256(&key, file_id, &plain_sha),
            Some([
                0xa3, 0xc8, 0x14, 0xe7, 0x85, 0xac, 0x7f, 0xb3, 0x6e, 0xa8, 0x4b, 0x78, 0x11, 0x0e,
                0x65, 0x68, 0x24, 0xb9, 0xce, 0x6d, 0x1b, 0xdd, 0x55, 0xd7, 0x49, 0x58, 0x1c, 0xe2,
                0x9a, 0x1c, 0x58, 0xcf,
            ]),
        );
    }
}
