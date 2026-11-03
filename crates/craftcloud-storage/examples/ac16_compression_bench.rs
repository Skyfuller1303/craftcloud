//! AC-16 measurements. Input files are only read; no content is written or logged.

use craftcloud_storage::{
    compression::{decode_chunk, encode_chunk},
    fastcdc_chunks,
};
use sha2::{Digest, Sha256};
use std::{error::Error, time::Instant};

fn main() -> Result<(), Box<dyn Error>> {
    let level = std::env::var("AC16_BENCH_LEVEL")
        .unwrap_or_else(|_| "3".to_owned())
        .parse::<i32>()?;
    if !(0..=19).contains(&level) {
        return Err("AC16_BENCH_LEVEL must be between 0 and 19".into());
    }
    let mut text = Vec::with_capacity(16 * 1024 * 1024);
    let row = b"<svg><text x=\"10\" y=\"20\">Synthetic AC16 compression measurement</text></svg>\n";
    while text.len() < 16 * 1024 * 1024 {
        text.extend_from_slice(row);
    }
    text.truncate(16 * 1024 * 1024);
    measure("synthetic-svg-text", &text, level)?;

    let mut state = 0x8e4c_a76b_1592_038du64;
    let random: Vec<u8> = (0..16 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    measure("synthetic-random", &random, level)?;

    // This is a synthetic payload analogue, not a real .pcraft file.
    let mut tiles = Vec::new();
    for ordinal in 0..64u16 {
        let tile: Vec<u8> = (0..256 * 256 * 4)
            .map(|position| ((position / 256 + position % 256 + usize::from(ordinal)) % 256) as u8)
            .collect();
        let encoded = encode_chunk(&tile, 3).map_err(|error| format!("tile: {error:?}"))?;
        tiles.extend_from_slice(&encoded.bytes);
    }
    measure("synthetic-precompressed-tiles-not-pcraft", &tiles, level)?;

    for path in std::env::args_os().skip(1) {
        let source = std::path::Path::new(&path);
        let bytes = std::fs::read(source)?;
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown");
        // Avoid persisting personal paths or file contents in benchmark output.
        measure(&format!("real-{extension}"), &bytes, level)?;
    }
    Ok(())
}

fn measure(label: &str, input: &[u8], level: i32) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let chunks = fastcdc_chunks(input)?;
    let chunking_seconds = started.elapsed().as_secs_f64();
    let mut stored_bytes = 0usize;
    let mut raw_chunks = 0usize;
    let mut zstd_chunks = 0usize;
    let mut encode_seconds = 0.0;
    let mut decode_verify_seconds = 0.0;
    for chunk in &chunks {
        let end = chunk
            .offset
            .checked_add(chunk.length)
            .ok_or("chunk boundary overflow")?;
        let plain = input
            .get(chunk.offset..end)
            .ok_or("invalid chunk boundary")?;
        let digest = Sha256::digest(plain);
        let started = Instant::now();
        let encoded = encode_chunk(plain, level).map_err(|error| format!("encode: {error:?}"))?;
        encode_seconds += started.elapsed().as_secs_f64();
        stored_bytes += encoded.bytes.len();
        if encoded.encoding == "zstd" {
            zstd_chunks += 1;
        } else {
            raw_chunks += 1;
        }
        let started = Instant::now();
        let decoded = decode_chunk(
            encoded.encoding,
            &encoded.bytes,
            plain.len() as u64,
            &digest,
        )
        .map_err(|error| format!("decode: {error:?}"))?;
        decode_verify_seconds += started.elapsed().as_secs_f64();
        if decoded != plain {
            return Err("decoded bytes differ".into());
        }
    }
    let mib = input.len() as f64 / (1024.0 * 1024.0);
    println!(
        "{{\"sample\":\"{label}\",\"level\":{level},\"logical_bytes\":{},\"stored_bytes\":{stored_bytes},\"stored_percent\":{:.6},\"chunks\":{},\"raw_chunks\":{raw_chunks},\"zstd_chunks\":{zstd_chunks},\"chunking_seconds\":{chunking_seconds:.6},\"encode_seconds\":{encode_seconds:.6},\"decode_verify_seconds\":{decode_verify_seconds:.6},\"encode_mib_s\":{:.3},\"decode_verify_mib_s\":{:.3},\"byte_exact\":true}}",
        input.len(),
        stored_bytes as f64 * 100.0 / input.len().max(1) as f64,
        chunks.len(),
        mib / encode_seconds.max(f64::EPSILON),
        mib / decode_verify_seconds.max(f64::EPSILON),
    );
    Ok(())
}
