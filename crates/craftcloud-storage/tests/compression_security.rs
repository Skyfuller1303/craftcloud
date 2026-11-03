//! Decoder boundaries independently exercised at the storage-format seam.

#![allow(clippy::expect_used)]

use craftcloud_storage::compression::{CompressionError, decode_chunk, encode_chunk};
use sha2::{Digest, Sha256};

#[test]
fn every_level_roundtrips_tiny_and_non_power_of_two_chunks() {
    for size in [1usize, 17, 255, 256, 1023, 1025, 65_791, 65_792, 1_700_003] {
        let plain = vec![b'x'; size];
        let digest = Sha256::digest(&plain);
        for level in 0..=19 {
            let encoded = encode_chunk(&plain, level).expect("valid compression level");
            let actual = decode_chunk(encoded.encoding, &encoded.bytes, size as u64, &digest)
                .expect("own frame remains readable under exact size and window limits");
            assert_eq!(actual, plain, "size={size}, level={level}");
        }
    }
}

#[test]
fn streaming_frame_without_content_size_obeys_output_and_window_limits() {
    // Standard frame, unknown content size, 1-KiB window, final raw block.
    let plain = vec![b'a'; 4096];
    let digest = Sha256::digest(&plain);
    let frame = frame_without_content_size(0, 4096, false, &plain);
    assert_eq!(
        decode_chunk("zstd", &frame, 4096, &digest).expect("bounded streaming frame"),
        plain
    );

    // Identical bounded metadata, but a final RLE block expands beyond it.
    let bomb = frame_without_content_size(0, 2048, true, b"a");
    assert!(decode_chunk("zstd", &bomb, 1024, &Sha256::digest(vec![b'a'; 1024])).is_err());
}

#[test]
fn excessive_window_is_rejected_even_when_output_would_fit() {
    let plain = vec![b'a'; 4096];
    let digest = Sha256::digest(&plain);
    // Exponent 3 declares an 8-KiB window for a 4-KiB logical chunk.
    let oversized_window = frame_without_content_size(3 << 3, 4096, true, b"a");
    assert_eq!(
        decode_chunk("zstd", &oversized_window, 4096, &digest),
        Err(CompressionError::TooLarge)
    );

    // Even the minimum non-single-segment window exceeds a tiny chunk.
    let tiny = frame_without_content_size(0, 32, true, b"a");
    assert_eq!(
        decode_chunk("zstd", &tiny, 32, &Sha256::digest(vec![b'a'; 32])),
        Err(CompressionError::TooLarge)
    );
}

#[test]
fn malformed_header_variants_and_truncations_fail_closed() {
    let plain = vec![b'a'; 4096];
    let digest = Sha256::digest(&plain);
    let frame = frame_without_content_size(0, 4096, true, b"a");
    for length in 0..frame.len() {
        let truncated = frame.get(..length).expect("bounded truncation");
        assert!(decode_chunk("zstd", truncated, 4096, &digest).is_err());
    }
    for descriptor in [0x08u8, 0x10, 0x18, 0xff] {
        let mut malformed = frame.clone();
        *malformed.get_mut(4).expect("frame descriptor") = descriptor;
        assert!(decode_chunk("zstd", &malformed, 4096, &digest).is_err());
    }
    let mut bad_magic = frame;
    *bad_magic.first_mut().expect("frame magic") ^= 0xff;
    assert!(decode_chunk("zstd", &bad_magic, 4096, &digest).is_err());
}

fn frame_without_content_size(window: u8, output_size: u32, rle: bool, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0, window];
    let header = (output_size << 3) | if rle { 3 } else { 1 };
    frame.extend_from_slice(
        header
            .to_le_bytes()
            .get(..3)
            .expect("three-byte block header"),
    );
    frame.extend_from_slice(payload);
    frame
}
