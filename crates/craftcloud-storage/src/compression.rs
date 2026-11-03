//! Bounded, per-chunk zstd storage encoding (AC-16).

use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

const MIN_SAVING_PERCENT: usize = 5;
static COMPRESSION_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn compression_slots() -> Arc<Semaphore> {
    Arc::clone(COMPRESSION_SLOTS.get_or_init(|| Arc::new(Semaphore::new(2))))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionError {
    InvalidLevel,
    InvalidEncoding,
    InvalidFrame,
    TooLarge,
    ChecksumMismatch,
    Codec,
}

impl std::fmt::Display for CompressionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLevel => "invalid compression level",
            Self::InvalidEncoding => "invalid chunk encoding",
            Self::InvalidFrame => "invalid compressed chunk frame",
            Self::TooLarge => "compressed chunk exceeds size limit",
            Self::ChecksumMismatch => "chunk checksum mismatch",
            Self::Codec => "chunk codec failure",
        })
    }
}

impl std::error::Error for CompressionError {}

/// Runs compression and its write-time roundtrip check on the blocking pool.
pub async fn encode_chunk_bounded(
    plaintext: Vec<u8>,
    level: i32,
) -> Result<EncodedChunk, CompressionError> {
    let permit = compression_slots()
        .acquire_owned()
        .await
        .map_err(|_| CompressionError::Codec)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let encoded = encode_chunk(&plaintext, level)?;
        let digest = Sha256::digest(&plaintext);
        decode_chunk(
            encoded.encoding,
            &encoded.bytes,
            plaintext.len() as u64,
            &digest,
        )?;
        Ok(encoded)
    })
    .await
    .map_err(|_| CompressionError::Codec)?
}

/// Runs bounded decompression on the blocking pool. Permit lives through the
/// closure even when caller cancels the future.
pub async fn decode_chunk_bounded(
    encoding: String,
    stored: Vec<u8>,
    size_bytes: u64,
    sha256: [u8; 32],
) -> Result<Vec<u8>, CompressionError> {
    let permit = compression_slots()
        .acquire_owned()
        .await
        .map_err(|_| CompressionError::Codec)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_chunk(&encoding, &stored, size_bytes, &sha256)
    })
    .await
    .map_err(|_| CompressionError::Codec)?
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedChunk {
    pub encoding: &'static str,
    pub bytes: Vec<u8>,
}

fn validate_frame_limits(frame: &[u8], size_bytes: u64) -> Result<(), CompressionError> {
    if frame.get(..4) != Some(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Err(CompressionError::InvalidFrame);
    }
    let descriptor = *frame.get(4).ok_or(CompressionError::InvalidFrame)?;
    if descriptor & 0x18 != 0 {
        return Err(CompressionError::InvalidFrame);
    }
    let single_segment = descriptor & 0x20 != 0;
    let window = if single_segment {
        None
    } else {
        let wd = *frame.get(5).ok_or(CompressionError::InvalidFrame)?;
        let log = 10u32
            .checked_add(u32::from(wd >> 3))
            .ok_or(CompressionError::InvalidFrame)?;
        let base = 1u64
            .checked_shl(log)
            .ok_or(CompressionError::InvalidFrame)?;
        let addition = (base >> 3)
            .checked_mul(u64::from(wd & 7))
            .ok_or(CompressionError::InvalidFrame)?;
        Some(
            base.checked_add(addition)
                .ok_or(CompressionError::InvalidFrame)?,
        )
    };
    if window.is_some_and(|window| window > size_bytes) {
        return Err(CompressionError::TooLarge);
    }
    let content_size = zstd::zstd_safe::get_frame_content_size(frame)
        .map_err(|_| CompressionError::InvalidFrame)?;
    if content_size.is_some_and(|content_size| content_size > size_bytes) {
        return Err(CompressionError::TooLarge);
    }
    if content_size.is_some_and(|content_size| content_size != size_bytes) {
        return Err(CompressionError::InvalidFrame);
    }
    Ok(())
}

/// Compresses only when zstd saves at least 5 percent. Level 0 means raw.
pub fn encode_chunk(plaintext: &[u8], level: i32) -> Result<EncodedChunk, CompressionError> {
    if !(0..=19).contains(&level) {
        return Err(CompressionError::InvalidLevel);
    }
    if plaintext.is_empty() || plaintext.len() > crate::MAX_CHUNK_BYTES as usize {
        return Err(CompressionError::TooLarge);
    }
    if level == 0 {
        return Ok(EncodedChunk {
            encoding: "raw",
            bytes: plaintext.to_vec(),
        });
    }
    let compressed = zstd::bulk::compress(plaintext, level).map_err(|_| CompressionError::Codec)?;
    if compressed.len().saturating_mul(100)
        <= plaintext.len().saturating_mul(100 - MIN_SAVING_PERCENT)
    {
        Ok(EncodedChunk {
            encoding: "zstd",
            bytes: compressed,
        })
    } else {
        Ok(EncodedChunk {
            encoding: "raw",
            bytes: plaintext.to_vec(),
        })
    }
}

/// Decodes exactly one bounded frame and checks its plaintext digest.
pub fn decode_chunk(
    encoding: &str,
    stored: &[u8],
    size_bytes: u64,
    sha256: &[u8],
) -> Result<Vec<u8>, CompressionError> {
    let expected_size = usize::try_from(size_bytes).map_err(|_| CompressionError::TooLarge)?;
    if size_bytes == 0
        || size_bytes > crate::MAX_CHUNK_BYTES as u64
        || stored.len() > crate::MAX_CHUNK_BYTES as usize
    {
        return Err(CompressionError::TooLarge);
    }
    if sha256.len() != 32 {
        return Err(CompressionError::ChecksumMismatch);
    }
    let plaintext = match encoding {
        "raw" => {
            if stored.len() != expected_size {
                return Err(CompressionError::InvalidFrame);
            }
            stored.to_vec()
        }
        "zstd" => {
            validate_frame_limits(stored, size_bytes)?;
            let frame_size = zstd::zstd_safe::find_frame_compressed_size(stored)
                .map_err(|_| CompressionError::InvalidFrame)?;
            if frame_size != stored.len() {
                return Err(CompressionError::InvalidFrame);
            }
            let mut decoder =
                zstd::bulk::Decompressor::new().map_err(|_| CompressionError::Codec)?;
            let window_log = 64 - size_bytes.leading_zeros();
            decoder
                .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(
                    window_log.max(10),
                ))
                .map_err(|_| CompressionError::InvalidFrame)?;
            decoder
                .decompress(stored, expected_size)
                .map_err(|_| CompressionError::TooLarge)?
        }
        _ => return Err(CompressionError::InvalidEncoding),
    };
    if plaintext.len() != expected_size {
        return Err(CompressionError::InvalidFrame);
    }
    let digest = Sha256::digest(&plaintext);
    if digest.as_slice() != sha256 {
        return Err(CompressionError::ChecksumMismatch);
    }
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_savings_and_keeps_incompressible_bytes_raw() {
        let repeated = vec![b'a'; 128 * 1024];
        let result = encode_chunk(&repeated, 3);
        assert!(result.is_ok());
        let encoded = result.unwrap_or_else(|_| EncodedChunk {
            encoding: "raw",
            bytes: Vec::new(),
        });
        assert_eq!(encoded.encoding, "zstd");
        assert!(encoded.bytes.len() < repeated.len());
        assert_eq!(
            decode_chunk(
                "zstd",
                &encoded.bytes,
                repeated.len() as u64,
                &Sha256::digest(&repeated)
            ),
            Ok(repeated)
        );
        let mut state = 0x16ac_0033_u32;
        let randomish = (0..128 * 1024)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect::<Vec<_>>();
        let result = encode_chunk(&randomish, 3);
        assert!(result.is_ok());
        let encoded = result.unwrap_or_else(|_| EncodedChunk {
            encoding: "zstd",
            bytes: Vec::new(),
        });
        assert_eq!(encoded.encoding, "raw");
        assert_eq!(
            decode_chunk(
                "raw",
                &encoded.bytes,
                randomish.len() as u64,
                &Sha256::digest(&randomish)
            ),
            Ok(randomish)
        );
    }

    #[test]
    fn rejects_expansion_digest_and_concatenated_frames() {
        let plaintext = vec![b'x'; 64 * 1024];
        let compressed = zstd::bulk::compress(&plaintext, 3);
        assert!(compressed.is_ok());
        let encoded = compressed.unwrap_or_default();
        let digest = Sha256::digest(&plaintext);
        assert_eq!(
            decode_chunk("zstd", &encoded, (plaintext.len() - 1) as u64, &digest),
            Err(CompressionError::TooLarge)
        );
        let mut wrong = digest.to_vec();
        wrong[0] ^= 1;
        assert_eq!(
            decode_chunk("zstd", &encoded, plaintext.len() as u64, &wrong),
            Err(CompressionError::ChecksumMismatch)
        );
        let mut concat = encoded.clone();
        concat.extend_from_slice(&encoded);
        assert_eq!(
            decode_chunk("zstd", &concat, plaintext.len() as u64, &digest),
            Err(CompressionError::InvalidFrame)
        );
    }

    #[test]
    fn refuses_oversized_plaintext_before_codec_allocation() {
        let oversized = vec![0; crate::MAX_CHUNK_BYTES as usize + 1];
        assert_eq!(encode_chunk(&oversized, 3), Err(CompressionError::TooLarge));
    }
}
