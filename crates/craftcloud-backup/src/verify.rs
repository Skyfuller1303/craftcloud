//! Offline verification of a backup directory.
//!
//! `verify` needs no database and no store. It recomputes every checksum and
//! rejects a missing, extra, altered or wrong-sized entry, an unknown format
//! or migration version, and a manifest whose own checksum does not match.
//! Memory is bounded: files are hashed in 64 KiB blocks and the blob list is
//! read line by line (each line at most [`MAX_BLOB_LINE_BYTES`]).
//!
//! The checksum file next to the manifest detects damage, not forgery: whoever
//! can change the backup can change both. To detect tampering, keep the
//! manifest digest that `backup create` prints somewhere else and pass it as
//! the expected digest.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::archive::{ArchiveAccess, verify_remote_chunks, verify_remote_refs};
use crate::error::{BackupError, ErrorKind};
use crate::manifest::{
    BLOB_INDEX_FILE, BLOBS_DIR, BlobEntry, CHUNK_INDEX_FILE, CHUNKED_VERSION_INDEX_FILE,
    CHUNKS_DIR, ChunkEntry, MANIFEST_FILE, MANIFEST_SUM_FILE, MAX_BLOB_LINE_BYTES,
    MAX_MANIFEST_BYTES, Manifest, TABLES_DIR, VERSION_CHUNK_INDEX_FILE, blob_file, hex,
    is_sha256_hex, parse_sha256,
};

/// Block size for hashing and copying.
pub(crate) const BLOCK: usize = 64 * 1024;

/// What a successful verification found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// The checked manifest.
    pub manifest: Manifest,
    /// SHA-256 of `manifest.json`, lowercase hex.
    pub manifest_sha256: String,
    /// Rows over all tables.
    pub rows: u64,
    /// Number of blob files.
    pub blobs: u64,
    /// Bytes over all blob files.
    pub blob_bytes: u64,
}

/// Verifies the local backup structure and, when archive references exist,
/// checks every remote object plus a deterministic decrypted sample.
pub async fn verify_with_archive(
    dir: &Path,
    expected_manifest_sha256: Option<&str>,
    archive: ArchiveAccess<'_>,
) -> Result<VerifyReport, BackupError> {
    let verify_dir = dir.to_path_buf();
    let expected = expected_manifest_sha256.map(str::to_owned);
    let report = tokio::task::spawn_blocking(move || verify(&verify_dir, expected.as_deref()))
        .await
        .map_err(|_| BackupError::new(ErrorKind::Io, "the verification task failed"))??;
    let mut entries = crate::blob_stream::blob_entries(dir);
    let mut archived = false;
    while let Some(entry) = entries.next().await? {
        archived |= entry.archive_object_key.is_some();
    }
    let _ = entries.summary().ok_or_else(|| {
        BackupError::new(ErrorKind::Io, "the blob list verification was incomplete")
    })?;
    if archived {
        let backup_id = report
            .manifest
            .backup_id
            .as_deref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .ok_or_else(|| {
                BackupError::new(ErrorKind::Format, "the archive backup id is invalid")
            })?;
        let remote = verify_remote_refs(archive, backup_id, dir).await?;
        if Some(remote.sha256) != parse_sha256(&report.manifest.blobs.sha256)
            || remote.count != report.manifest.blobs.count
            || remote.total_bytes != report.manifest.blobs.total_bytes
        {
            return Err(integrity(
                "the remote verification did not match the blob list",
            ));
        }
    }
    if report.manifest.chunks.is_some() {
        verify_remote_chunks(
            archive,
            report
                .manifest
                .backup_id
                .as_deref()
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .unwrap_or_default(),
            dir,
        )
        .await?;
    }
    verify_archived_version_assemblies(dir, archive).await?;
    Ok(report)
}

/// Performs restore's complete read-only preflight before opening a target
/// store or database for writes. Archived references require a provider.
pub async fn preflight_restore_archive(
    dir: &Path,
    expected_manifest_sha256: Option<&str>,
    archive: Option<ArchiveAccess<'_>>,
) -> Result<VerifyReport, BackupError> {
    let verify_dir = dir.to_path_buf();
    let expected = expected_manifest_sha256.map(str::to_owned);
    let report = tokio::task::spawn_blocking(move || verify(&verify_dir, expected.as_deref()))
        .await
        .map_err(|_| BackupError::new(ErrorKind::Io, "the verification task failed"))??;
    let mut entries = crate::blob_stream::blob_entries(dir);
    let mut archived = false;
    while let Some(entry) = entries.next().await? {
        archived |= entry.archive_object_key.is_some();
    }
    let _ = entries.summary().ok_or_else(|| {
        BackupError::new(ErrorKind::Io, "the blob list verification was incomplete")
    })?;
    if archived {
        let access = archive.ok_or_else(|| {
            BackupError::new(
                ErrorKind::SourceDamaged,
                "restore of archived blobs requires the configured provider and server key",
            )
        })?;
        let backup_id = report
            .manifest
            .backup_id
            .as_deref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .ok_or_else(|| {
                BackupError::new(ErrorKind::Format, "the archive backup id is invalid")
            })?;
        let remote = verify_remote_refs(access, backup_id, dir).await?;
        if Some(remote.sha256) != parse_sha256(&report.manifest.blobs.sha256)
            || remote.count != report.manifest.blobs.count
            || remote.total_bytes != report.manifest.blobs.total_bytes
        {
            return Err(integrity(
                "the remote verification did not match the blob list",
            ));
        }
    }
    if report.manifest.chunks.is_some() {
        let has_archive = {
            let mut found = false;
            for_each_chunk(dir, |entry| {
                found |= entry.archive_object_key.is_some();
                Ok(())
            })?;
            found
        };
        if has_archive {
            let access = archive.ok_or_else(|| {
                BackupError::new(
                    ErrorKind::SourceDamaged,
                    "restore of archived chunks requires the configured provider and server key",
                )
            })?;
            let id = report
                .manifest
                .backup_id
                .as_deref()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .ok_or_else(|| {
                    BackupError::new(ErrorKind::Format, "the archive backup id is invalid")
                })?;
            verify_remote_chunks(access, id, dir).await?;
        }
    }
    if let Some(access) = archive {
        verify_archived_version_assemblies(dir, access).await?;
    }
    Ok(report)
}

async fn verify_archived_version_assemblies(
    dir: &Path,
    access: ArchiveAccess<'_>,
) -> Result<(), BackupError> {
    use crate::manifest::{ChunkedVersionEntry, VersionChunkEntry};
    let Some(versions_path) = dir
        .join(CHUNKED_VERSION_INDEX_FILE)
        .exists()
        .then(|| dir.join(CHUNKED_VERSION_INDEX_FILE))
    else {
        return Ok(());
    };
    let mut expected = std::collections::BTreeMap::new();
    let mut versions_reader = std::io::BufReader::new(
        File::open(versions_path)
            .map_err(|e| BackupError::io("cannot open chunked version list", &e))?,
    );
    let mut line = Vec::new();
    while read_jsonl_line(&mut versions_reader, &mut line, "chunked version list")? {
        let entry = ChunkedVersionEntry::parse_line(&line)?;
        expected.insert(entry.id, (entry.size, entry.sha256));
    }
    let mappings = File::open(dir.join(VERSION_CHUNK_INDEX_FILE))
        .map_err(|e| BackupError::io("cannot open version chunk list", &e))?;
    let mut current_id = String::new();
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut ordinal = 0_u32;
    async fn finish(
        id: &str,
        size: u64,
        hasher: &mut Sha256,
        expected: &std::collections::BTreeMap<String, (u64, String)>,
    ) -> Result<(), BackupError> {
        if id.is_empty() {
            return Ok(());
        }
        let Some((expected_size, expected_sha)) = expected.get(id) else {
            return Err(integrity("a chunk mapping references an unknown version"));
        };
        let actual: [u8; 32] = hasher.clone().finalize().into();
        if size != *expected_size || hex(&actual) != *expected_sha {
            return Err(integrity(
                "assembled archived version does not match its size or SHA-256",
            ));
        }
        Ok(())
    }
    let mut mappings_reader = std::io::BufReader::new(mappings);
    let mut line = Vec::new();
    while read_jsonl_line(&mut mappings_reader, &mut line, "version chunk list")? {
        let entry = VersionChunkEntry::parse_line(&line)?;
        if current_id != entry.version_id {
            finish(&current_id, size, &mut hasher, &expected).await?;
            current_id = entry.version_id.clone();
            hasher = Sha256::new();
            size = 0;
            ordinal = 0;
        }
        if entry.ordinal != ordinal || entry.byte_offset != size {
            return Err(integrity(
                "version chunk mappings have a discontinuous ordinal or offset",
            ));
        }
        let bytes = if entry.chunk.local_copy {
            std::fs::read(dir.join(crate::manifest::chunk_file(&entry.chunk.id)))
                .map_err(|e| BackupError::io("cannot read mapped chunk", &e))?
        } else {
            crate::archive::read_archive_chunk(access.store, access.crypto, &entry.chunk).await?
        };
        hasher.update(&bytes);
        size = size
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| integrity("assembled version size overflow"))?;
        ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| integrity("chunk ordinal overflow"))?;
    }
    finish(&current_id, size, &mut hasher, &expected).await?;
    Ok(())
}

pub(crate) fn integrity(message: impl Into<String>) -> BackupError {
    BackupError::new(ErrorKind::Integrity, message)
}

/// The result of hashing a file.
pub(crate) struct FileSum {
    pub len: u64,
    pub sha256: [u8; 32],
    /// Number of `\n` bytes.
    pub newlines: u64,
}

/// A plain file (not a link, not a directory) or an integrity error.
pub(crate) fn require_file(dir: &Path, relative: &str) -> Result<fs::Metadata, BackupError> {
    let path = dir.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(meta),
        Ok(_) => Err(integrity(format!("{relative} is not a regular file"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(integrity(format!("{relative} is missing")))
        }
        Err(error) => Err(BackupError::io(&format!("cannot read {relative}"), &error)),
    }
}

/// Hashes a file in 64 KiB blocks.
pub(crate) fn sum_file(dir: &Path, relative: &str) -> Result<FileSum, BackupError> {
    let path = dir.join(relative);
    let mut file = File::open(&path)
        .map_err(|error| BackupError::io(&format!("cannot open {relative}"), &error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; BLOCK];
    let (mut len, mut newlines) = (0_u64, 0_u64);
    loop {
        let got = file
            .read(&mut buffer)
            .map_err(|error| BackupError::io(&format!("cannot read {relative}"), &error))?;
        if got == 0 {
            break;
        }
        let block = buffer.get(..got).unwrap_or_default();
        hasher.update(block);
        len += got as u64;
        newlines += block.iter().filter(|b| **b == b'\n').count() as u64;
    }
    Ok(FileSum {
        len,
        sha256: hasher.finalize().into(),
        newlines,
    })
}

/// Reads one bounded JSONL record. The reader consumes at most two bytes
/// beyond the accepted payload, even when the input has no newline.
fn read_jsonl_line<R: std::io::BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    what: &'static str,
) -> Result<bool, BackupError> {
    line.clear();
    let got = reader
        .by_ref()
        .take(MAX_BLOB_LINE_BYTES as u64 + 2)
        .read_until(b'\n', line)
        .map_err(|error| BackupError::io(&format!("cannot read {what}"), &error))?;
    if got == 0 {
        return Ok(false);
    }
    if line.pop() != Some(b'\n') || line.len() > MAX_BLOB_LINE_BYTES {
        return Err(BackupError::new(
            ErrorKind::Format,
            format!("{what} has an overlong or unterminated line"),
        ));
    }
    Ok(true)
}

/// Reads the whole manifest (bounded) and returns its bytes.
fn read_bounded(dir: &Path, relative: &str, limit: u64) -> Result<Vec<u8>, BackupError> {
    let meta = require_file(dir, relative)?;
    if meta.len() > limit {
        return Err(BackupError::new(
            ErrorKind::Format,
            format!("{relative} is too large"),
        ));
    }
    let mut bytes = Vec::new();
    File::open(dir.join(relative))
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|error| BackupError::io(&format!("cannot read {relative}"), &error))?;
    Ok(bytes)
}

/// What was read from `blobs.jsonl` while it was streamed.
#[derive(Debug)]
pub(crate) struct BlobListSum {
    /// SHA-256 of the bytes that were read.
    pub sha256: [u8; 32],
    /// Number of entries.
    pub count: u64,
    /// Sum of the entry sizes.
    pub total_bytes: u64,
}

/// Calls `each` for every entry of `blobs.jsonl`, reading line by line, and
/// returns the checksum, count and size total of what it read (so a caller
/// that acts on the entries can compare them with the manifest and does not
/// have to trust a file that changed after `verify`). Stops at the first
/// error. Used by verify and restore.
pub(crate) fn for_each_blob<F>(dir: &Path, mut each: F) -> Result<BlobListSum, BackupError>
where
    F: FnMut(&BlobEntry) -> Result<(), BackupError>,
{
    let file = File::open(dir.join(BLOB_INDEX_FILE))
        .map_err(|error| BackupError::io("cannot open the blob list", &error))?;
    let mut reader = BufReader::with_capacity(BLOCK, file);
    let mut line = Vec::new();
    let mut hasher = Sha256::new();
    let (mut count, mut total_bytes) = (0_u64, 0_u64);
    loop {
        line.clear();
        // One byte more than allowed, so an overlong line is noticed.
        let got = reader
            .by_ref()
            .take(MAX_BLOB_LINE_BYTES as u64 + 2)
            .read_until(b'\n', &mut line)
            .map_err(|error| BackupError::io("cannot read the blob list", &error))?;
        if got == 0 {
            return Ok(BlobListSum {
                sha256: hasher.finalize().into(),
                count,
                total_bytes,
            });
        }
        hasher.update(&line);
        if line.last() == Some(&b'\n') {
            line.pop();
        } else {
            return Err(BackupError::new(
                ErrorKind::Format,
                "the blob list has an overlong or unterminated line",
            ));
        }
        let entry = BlobEntry::parse_line(&line)?;
        count += 1;
        total_bytes = total_bytes
            .checked_add(entry.size)
            .ok_or_else(|| integrity("the blob sizes overflow"))?;
        each(&entry)?;
    }
}

/// Reads and checks the manifest and its checksum.
///
/// # Errors
///
/// [`ErrorKind::Integrity`] when a checksum does not match or the checksum
/// file is unusable, [`ErrorKind::Format`] for an unknown or malformed
/// manifest.
pub fn read_manifest(
    dir: &Path,
    expected_manifest_sha256: Option<&str>,
) -> Result<(Manifest, String), BackupError> {
    let bytes = read_bounded(dir, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
    let actual = hex(&Sha256::digest(&bytes));
    if let Some(expected) = expected_manifest_sha256 {
        if !is_sha256_hex(expected) {
            return Err(BackupError::new(
                ErrorKind::Format,
                "the expected manifest digest must be 64 lowercase hex characters",
            ));
        }
        if expected != actual {
            return Err(integrity(
                "manifest.json does not match the expected manifest digest",
            ));
        }
    }
    let sum_text = read_bounded(dir, MANIFEST_SUM_FILE, 256)?;
    let recorded = String::from_utf8_lossy(&sum_text)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    if !is_sha256_hex(&recorded) {
        return Err(integrity("manifest.sha256 is malformed"));
    }
    if recorded != actual {
        return Err(integrity(
            "manifest.json does not match manifest.sha256 (the manifest was changed or damaged)",
        ));
    }
    let manifest = Manifest::parse(&bytes, &craftcloud_db::migrate::embedded_versions())?;
    Ok((manifest, actual))
}

/// Verifies a backup directory completely (blocking; call it from a blocking
/// task in async code).
///
/// # Errors
///
/// [`BackupError`] with kind [`ErrorKind::Integrity`] or [`ErrorKind::Format`]
/// naming the first problem found.
pub fn verify(
    dir: &Path,
    expected_manifest_sha256: Option<&str>,
) -> Result<VerifyReport, BackupError> {
    let meta = fs::symlink_metadata(dir)
        .map_err(|error| BackupError::io("cannot read the backup directory", &error))?;
    if !meta.is_dir() {
        return Err(integrity("the backup path is not a directory"));
    }
    // The two directories must be real directories: a link could point the
    // checks (and a later restore) at files outside the backup.
    for name in [TABLES_DIR, BLOBS_DIR] {
        match fs::symlink_metadata(dir.join(name)) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(integrity(format!(
                    "{name} must be a real directory (links are not allowed)"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(integrity(format!("the {name} directory is missing")));
            }
            Err(error) => {
                return Err(BackupError::io(&format!("cannot read {name}"), &error));
            }
        }
    }
    let (manifest, manifest_sha256) = read_manifest(dir, expected_manifest_sha256)?;
    if manifest.chunks.is_some() {
        match fs::symlink_metadata(dir.join(CHUNKS_DIR)) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(integrity(
                    "chunks must be a real directory (links are not allowed)",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(integrity("the chunks directory is missing"));
            }
            Err(error) => return Err(BackupError::io("cannot read chunks", &error)),
        }
    } else if dir.join(CHUNK_INDEX_FILE).exists() || dir.join(CHUNKS_DIR).exists() {
        return Err(integrity("this backup format does not list chunk files"));
    }
    if manifest.chunked_versions.is_none()
        && (dir.join(CHUNKED_VERSION_INDEX_FILE).exists()
            || dir.join(VERSION_CHUNK_INDEX_FILE).exists())
    {
        return Err(integrity(
            "this backup format does not list chunked version indexes",
        ));
    }

    // Tables: size, row count and checksum.
    let mut rows = 0_u64;
    for table in &manifest.tables {
        require_file(dir, &table.file)?;
        let sum = sum_file(dir, &table.file)?;
        if sum.len != table.bytes {
            return Err(integrity(format!(
                "{} has the wrong size (manifest {}, file {})",
                table.file, table.bytes, sum.len
            )));
        }
        if hex(&sum.sha256) != table.sha256 {
            return Err(integrity(format!(
                "{} does not match its checksum",
                table.file
            )));
        }
        // COPY text format escapes newlines inside values: one line, one row.
        if sum.newlines != table.rows {
            return Err(integrity(format!(
                "{} has {} rows but the manifest says {}",
                table.file, sum.newlines, table.rows
            )));
        }
        rows += table.rows;
    }

    // The blob list itself.
    require_file(dir, BLOB_INDEX_FILE)?;
    let list = sum_file(dir, BLOB_INDEX_FILE)?;
    if hex(&list.sha256) != manifest.blobs.sha256 {
        return Err(integrity("blobs.jsonl does not match its checksum"));
    }
    if list.newlines != manifest.blobs.count {
        return Err(integrity(format!(
            "blobs.jsonl lists {} blobs but the manifest says {}",
            list.newlines, manifest.blobs.count
        )));
    }

    // Every listed blob: ascending unique keys, size and checksum.
    let (mut count, mut total, mut local_count) = (0_u64, 0_u64, 0_u64);
    let mut previous: Option<String> = None;
    for_each_blob(dir, |entry| {
        if previous.as_deref().is_some_and(|p| p >= entry.key.as_str()) {
            return Err(integrity("blobs.jsonl is not sorted or repeats a key"));
        }
        previous = Some(entry.key.clone());
        if entry.local_copy {
            let relative = blob_file(&entry.key);
            require_file(dir, &relative)?;
            let sum = sum_file(dir, &relative)?;
            if sum.len != entry.size {
                return Err(integrity(format!(
                    "blob {} has the wrong size (list {}, file {})",
                    entry.key, entry.size, sum.len
                )));
            }
            if hex(&sum.sha256) != entry.sha256 {
                return Err(integrity(format!(
                    "blob {} does not match its checksum",
                    entry.key
                )));
            }
            local_count += 1;
        }
        count += 1;
        total = total
            .checked_add(entry.size)
            .ok_or_else(|| integrity("the blob sizes overflow"))?;
        Ok(())
    })?;
    if count != manifest.blobs.count || total != manifest.blobs.total_bytes {
        return Err(integrity(
            "the blob list does not match the blob count or total size of the manifest",
        ));
    }

    let mut chunk_count = 0_u64;
    if let Some(index) = &manifest.chunks {
        require_file(dir, CHUNK_INDEX_FILE)?;
        let idx = sum_file(dir, CHUNK_INDEX_FILE)?;
        if hex(&idx.sha256) != index.sha256 || idx.newlines != index.count {
            return Err(integrity("chunks.jsonl does not match its manifest"));
        }
        let mut previous = String::new();
        let mut total_chunks = 0_u64;
        let mut local_chunks = 0_u64;
        for_each_chunk(dir, |entry| {
            if entry.id <= previous {
                return Err(integrity("chunks.jsonl is not sorted or repeats an id"));
            }
            previous.clone_from(&entry.id);
            if entry.local_copy {
                let relative = crate::manifest::chunk_file(&entry.id);
                require_file(dir, &relative)?;
                let sum = sum_file(dir, &relative)?;
                if sum.len != entry.size || hex(&sum.sha256) != entry.sha256 {
                    return Err(integrity(format!(
                        "chunk {} does not match its recorded checksum",
                        entry.id
                    )));
                }
                local_chunks += 1;
            }
            total_chunks = total_chunks
                .checked_add(entry.size)
                .ok_or_else(|| integrity("chunk sizes overflow"))?;
            chunk_count += 1;
            Ok(())
        })?;
        if chunk_count != index.count || total_chunks != index.total_bytes {
            return Err(integrity(
                "chunk list does not match its count or size total",
            ));
        }
        let _ = local_chunks;
    }

    if let (Some(versions), Some(mappings)) = (&manifest.chunked_versions, &manifest.version_chunks)
    {
        for (name, index) in [
            (CHUNKED_VERSION_INDEX_FILE, versions),
            (VERSION_CHUNK_INDEX_FILE, mappings),
        ] {
            require_file(dir, name)?;
            let sum = sum_file(dir, name)?;
            if hex(&sum.sha256) != index.sha256 || sum.newlines != index.count {
                return Err(integrity(format!("{name} does not match its manifest")));
            }
        }
        verify_chunked_index_contents(dir)?;
    }

    check_no_extra_files(dir, &manifest, local_count)?;
    Ok(VerifyReport {
        manifest,
        manifest_sha256,
        rows,
        blobs: count,
        blob_bytes: total,
    })
}

pub(crate) fn for_each_chunk<F>(dir: &Path, mut each: F) -> Result<(), BackupError>
where
    F: FnMut(&ChunkEntry) -> Result<(), BackupError>,
{
    let file = File::open(dir.join(CHUNK_INDEX_FILE))
        .map_err(|error| BackupError::io("cannot open the chunk list", &error))?;
    let mut reader = BufReader::with_capacity(BLOCK, file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let got = reader
            .by_ref()
            .take(MAX_BLOB_LINE_BYTES as u64 + 2)
            .read_until(b'\n', &mut line)
            .map_err(|error| BackupError::io("cannot read the chunk list", &error))?;
        if got == 0 {
            return Ok(());
        }
        if line.pop() != Some(b'\n') {
            return Err(BackupError::new(
                ErrorKind::Format,
                "the chunk list has an overlong or unterminated line",
            ));
        }
        each(&ChunkEntry::parse_line(&line)?)?;
    }
}

fn verify_chunked_index_contents(dir: &Path) -> Result<(), BackupError> {
    use crate::manifest::{ChunkedVersionEntry, VersionChunkEntry};
    let versions = File::open(dir.join(CHUNKED_VERSION_INDEX_FILE))
        .map_err(|e| BackupError::io("cannot open chunked version list", &e))?;
    let mut expected = std::collections::BTreeMap::new();
    let mut versions_reader = BufReader::new(versions);
    let mut line = Vec::new();
    while read_jsonl_line(&mut versions_reader, &mut line, "chunked version list")? {
        let entry = ChunkedVersionEntry::parse_line(&line)?;
        if expected
            .insert(entry.id, (entry.size, entry.sha256))
            .is_some()
        {
            return Err(integrity("chunked version list repeats an id"));
        }
    }
    let mappings = File::open(dir.join(VERSION_CHUNK_INDEX_FILE))
        .map_err(|e| BackupError::io("cannot open version chunk list", &e))?;
    let mut current: Option<(String, u32, u64, Sha256, bool)> = None;
    let mut seen = BTreeSet::new();
    let mut previous: Option<(String, u32)> = None;
    let mut mappings_reader = BufReader::new(mappings);
    let mut line = Vec::new();
    while read_jsonl_line(&mut mappings_reader, &mut line, "version chunk list")? {
        let entry = VersionChunkEntry::parse_line(&line)?;
        let key = (entry.version_id.clone(), entry.ordinal);
        if previous.as_ref().is_some_and(|last| last >= &key) {
            return Err(integrity(
                "version chunk list is not sorted or repeats a mapping",
            ));
        }
        previous = Some(key);
        if current
            .as_ref()
            .is_some_and(|item| item.0 != entry.version_id)
        {
            finish_chunked_version(current.take(), &expected, &mut seen)?;
        }
        if current.is_none() {
            current = Some((entry.version_id.clone(), 0, 0, Sha256::new(), false));
        }
        let (version, ordinal, offset, digest, archived) = current
            .as_mut()
            .ok_or_else(|| integrity("chunked version state is missing"))?;
        if entry.ordinal != *ordinal || entry.byte_offset != *offset {
            return Err(integrity(
                "version chunk mappings have a discontinuous ordinal or offset",
            ));
        }
        if entry.chunk.local_copy {
            let path = dir.join(crate::manifest::chunk_file(&entry.chunk.id));
            let mut file =
                File::open(path).map_err(|e| BackupError::io("cannot open mapped chunk", &e))?;
            let mut chunk_hash = Sha256::new();
            let mut buf = vec![0; BLOCK];
            let mut chunk_len = 0_u64;
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| BackupError::io("cannot read mapped chunk", &e))?;
                if n == 0 {
                    break;
                }
                chunk_len += n as u64;
                let block = buf
                    .get(..n)
                    .ok_or_else(|| integrity("mapped chunk read exceeded its buffer"))?;
                chunk_hash.update(block);
                digest.update(block);
            }
            if chunk_len != entry.chunk.size || hex(&chunk_hash.finalize()) != entry.chunk.sha256 {
                return Err(integrity("mapped chunk bytes do not match their digest"));
            }
        } else {
            *archived = true;
        }
        *ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| integrity("chunk ordinal overflow"))?;
        *offset = offset
            .checked_add(entry.chunk.size)
            .ok_or_else(|| integrity("chunk offset overflow"))?;
        *version = entry.version_id;
    }
    finish_chunked_version(current, &expected, &mut seen)?;
    for (id, (size, sha)) in expected {
        if !seen.contains(&id) && (size != 0 || sha != hex(&Sha256::digest([]))) {
            return Err(integrity(
                "a chunked version has no mappings but is not empty",
            ));
        }
    }
    Ok(())
}

fn finish_chunked_version(
    current: Option<(String, u32, u64, Sha256, bool)>,
    expected: &std::collections::BTreeMap<String, (u64, String)>,
    seen: &mut BTreeSet<String>,
) -> Result<(), BackupError> {
    let Some((id, _, size, digest, archived)) = current else {
        return Ok(());
    };
    let (expected_size, expected_sha) = expected
        .get(&id)
        .ok_or_else(|| integrity("a chunk mapping references an unknown version"))?;
    if size != *expected_size || (!archived && hex(&digest.finalize()) != *expected_sha) {
        return Err(integrity(
            "assembled chunked version does not match its size or SHA-256",
        ));
    }
    seen.insert(id);
    Ok(())
}

/// Nothing may be in the directory that the manifest does not list.
fn check_no_extra_files(dir: &Path, manifest: &Manifest, blobs: u64) -> Result<(), BackupError> {
    let top: BTreeSet<&str> = [
        MANIFEST_FILE,
        MANIFEST_SUM_FILE,
        BLOB_INDEX_FILE,
        CHUNK_INDEX_FILE,
        CHUNKED_VERSION_INDEX_FILE,
        VERSION_CHUNK_INDEX_FILE,
        TABLES_DIR,
        BLOBS_DIR,
        CHUNKS_DIR,
    ]
    .into_iter()
    .collect();
    let mut seen_dirs = 0_u8;
    for entry in read_dir(dir, "the backup directory")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !top.contains(name.as_str()) {
            return Err(integrity(format!("unexpected entry {name} in the backup")));
        }
        if name == TABLES_DIR
            || name == BLOBS_DIR
            || (name == CHUNKS_DIR && manifest.chunks.is_some())
        {
            seen_dirs += 1;
        }
    }
    if seen_dirs != if manifest.chunks.is_some() { 3 } else { 2 } {
        return Err(integrity("the tables or blobs directory is missing"));
    }

    let listed: BTreeSet<String> = manifest
        .tables
        .iter()
        .map(|t| t.file.trim_start_matches("tables/").to_owned())
        .collect();
    let tables_dir = dir.join(TABLES_DIR);
    let mut table_files = 0_usize;
    for entry in read_dir(&tables_dir, "tables")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !listed.contains(&name) {
            return Err(integrity(format!("unexpected file tables/{name}")));
        }
        table_files += 1;
    }
    if table_files != listed.len() {
        return Err(integrity("a table file is missing"));
    }

    // Blob files: every file below blobs/<xx>/ must be listed. All listed
    // files exist (checked above), so an equal count means no extra file.
    let mut found = 0_u64;
    for shard in read_dir(&dir.join(BLOBS_DIR), "blobs")? {
        let shard = shard?;
        let name = shard.file_name().to_string_lossy().into_owned();
        let valid = name.len() == 2 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let is_dir = shard.file_type().is_ok_and(|t| t.is_dir());
        if !valid || !is_dir {
            return Err(integrity(format!("unexpected entry blobs/{name}")));
        }
        for file in read_dir(&shard.path(), "a blob directory")? {
            let file = file?;
            let is_file = file.file_type().is_ok_and(|t| t.is_file());
            if !is_file {
                return Err(integrity(format!(
                    "unexpected entry in blobs/{name}: {}",
                    file.file_name().to_string_lossy()
                )));
            }
            found += 1;
        }
    }
    if found != blobs {
        return Err(integrity(format!(
            "the backup holds {found} blob files but lists {blobs}"
        )));
    }
    if let Some(index) = &manifest.chunks {
        let mut found_chunks = 0_u64;
        for file in read_dir(&dir.join(CHUNKS_DIR), "chunks")? {
            let file = file?;
            if !file.file_type().is_ok_and(|t| t.is_file()) {
                return Err(integrity("unexpected entry in chunks directory"));
            }
            found_chunks += 1;
        }
        let expected_local = {
            let mut n = 0_u64;
            for_each_chunk(dir, |e| {
                if e.local_copy {
                    n += 1;
                }
                Ok(())
            })?;
            n
        };
        if found_chunks != expected_local || index.count < expected_local {
            return Err(integrity("chunk directory does not match the chunk list"));
        }
    }
    if manifest.chunked_versions.is_some() {
        require_file(dir, CHUNKED_VERSION_INDEX_FILE)?;
        require_file(dir, VERSION_CHUNK_INDEX_FILE)?;
    }
    Ok(())
}

/// The entries of a directory, read lazily (bounded memory however many
/// files a shard directory holds).
fn read_dir(
    path: &Path,
    what: &'static str,
) -> Result<impl Iterator<Item = Result<fs::DirEntry, BackupError>>, BackupError> {
    let iter = fs::read_dir(path)
        .map_err(|error| BackupError::io(&format!("cannot read {what}"), &error))?;
    Ok(iter.map(move |entry| {
        entry.map_err(|error| BackupError::io(&format!("cannot read {what}"), &error))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        BlobIndex, FORMAT, FORMAT_VERSION, StoreInfo, TableEntry, blob_file, table_file,
    };
    use crate::tables::backup_tables_for;

    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("cc-verify-unit-{}", uuid::Uuid::new_v4().simple()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    #[test]
    fn oversized_jsonl_record_is_rejected_after_bounded_read() {
        let mut bytes = vec![b'x'; MAX_BLOB_LINE_BYTES + 1_000_000];
        bytes.push(b'\n');
        let mut reader = std::io::Cursor::new(bytes);
        let mut line = Vec::new();
        let error = read_jsonl_line(&mut reader, &mut line, "test index").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Format);
        assert!(line.len() <= MAX_BLOB_LINE_BYTES + 2);
        assert!(reader.position() <= (MAX_BLOB_LINE_BYTES + 2) as u64);
    }

    /// Writes a complete, valid backup of empty tables and the given blobs
    /// (sorted by key by this function).
    fn write_backup(dir: &Path, blobs: &[(&str, &[u8])]) {
        let mut blobs = blobs.to_vec();
        blobs.sort_by_key(|(key, _)| *key);
        fs::create_dir_all(dir.join(TABLES_DIR)).unwrap();
        fs::create_dir_all(dir.join(BLOBS_DIR)).unwrap();
        fs::create_dir_all(dir.join(CHUNKS_DIR)).unwrap();
        let mut tables = Vec::new();
        for spec in backup_tables_for(14).unwrap() {
            let content: &[u8] = if spec.name == "users" { b"row\n" } else { b"" };
            let file = table_file(spec.name);
            fs::write(dir.join(&file), content).unwrap();
            tables.push(TableEntry {
                name: spec.name.to_owned(),
                file,
                rows: content.len() as u64 / 4,
                bytes: content.len() as u64,
                sha256: sha(content),
            });
        }
        let mut list = Vec::new();
        let mut total = 0;
        for (key, content) in &blobs {
            let entry = BlobEntry {
                id: uuid::Uuid::new_v4().to_string(),
                key: (*key).to_owned(),
                size: content.len() as u64,
                layout: "whole".to_owned(),
                sha256: sha(content),
                local_copy: true,
                archive_object_key: None,
                archive_wrapped_key: None,
            };
            list.extend(serde_json::to_vec(&entry).unwrap());
            list.push(b'\n');
            let path = dir.join(blob_file(key));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            total += content.len() as u64;
        }
        fs::write(dir.join(BLOB_INDEX_FILE), &list).unwrap();
        fs::write(dir.join(CHUNK_INDEX_FILE), b"").unwrap();
        fs::write(dir.join(CHUNKED_VERSION_INDEX_FILE), b"").unwrap();
        fs::write(dir.join(VERSION_CHUNK_INDEX_FILE), b"").unwrap();
        let manifest = Manifest {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
            created_at: "2026-10-29T12:00:00Z".to_owned(),
            server_version: "0.0.0".to_owned(),
            migration_version: 14,
            store: StoreInfo {
                store_id: "x".to_owned(),
                epoch: 0,
            },
            tables,
            blobs: BlobIndex {
                file: BLOB_INDEX_FILE.to_owned(),
                count: blobs.len() as u64,
                total_bytes: total,
                sha256: sha(&list),
            },
            chunks: Some(crate::manifest::ChunkIndex {
                file: crate::manifest::CHUNK_INDEX_FILE.to_owned(),
                count: 0,
                total_bytes: 0,
                sha256: sha(b""),
            }),
            chunked_versions: Some(crate::manifest::JsonlIndex {
                file: crate::manifest::CHUNKED_VERSION_INDEX_FILE.to_owned(),
                count: 0,
                total_bytes: 0,
                sha256: sha(b""),
            }),
            version_chunks: Some(crate::manifest::JsonlIndex {
                file: crate::manifest::VERSION_CHUNK_INDEX_FILE.to_owned(),
                count: 0,
                total_bytes: 0,
                sha256: sha(b""),
            }),
            excluded_tables: Vec::new(),
            unavailable_version_blobs: 0,
            backup_id: Some(uuid::Uuid::new_v4().to_string()),
        };
        let bytes = manifest.to_bytes().unwrap();
        fs::write(dir.join(MANIFEST_FILE), &bytes).unwrap();
        fs::write(
            dir.join(MANIFEST_SUM_FILE),
            format!("{}  {MANIFEST_FILE}\n", sha(&bytes)),
        )
        .unwrap();
    }

    const KEY_A: &str = "aa000000000000000000000000000001";
    const KEY_B: &str = "bb000000000000000000000000000002";

    #[test]
    fn a_complete_backup_verifies() {
        let dir = Dir::new();
        write_backup(&dir.0, &[(KEY_B, b"second"), (KEY_A, b"first!")]);
        let report = verify(&dir.0, None).unwrap();
        assert_eq!((report.blobs, report.blob_bytes, report.rows), (2, 12, 1));
        assert!(verify(&dir.0, Some(&report.manifest_sha256)).is_ok());
        assert_eq!(
            verify(&dir.0, Some(&"f".repeat(64))).unwrap_err().kind,
            ErrorKind::Integrity
        );
        assert_eq!(
            verify(&dir.0, Some("not-hex")).unwrap_err().kind,
            ErrorKind::Format
        );
    }

    #[test]
    fn damage_is_named() {
        let dir = Dir::new();
        write_backup(&dir.0, &[(KEY_A, b"first!")]);
        let blob = dir.0.join(blob_file(KEY_A));

        fs::write(&blob, b"first?").unwrap();
        assert!(
            verify(&dir.0, None)
                .unwrap_err()
                .message
                .contains("checksum")
        );
        fs::write(&blob, b"first").unwrap();
        assert!(
            verify(&dir.0, None)
                .unwrap_err()
                .message
                .contains("wrong size")
        );
        fs::remove_file(&blob).unwrap();
        assert!(
            verify(&dir.0, None)
                .unwrap_err()
                .message
                .contains("missing")
        );
        fs::write(&blob, b"first!").unwrap();
        assert!(verify(&dir.0, None).is_ok());

        fs::write(dir.0.join("tables").join("stray.copy"), b"x").unwrap();
        assert!(
            verify(&dir.0, None)
                .unwrap_err()
                .message
                .contains("unexpected file")
        );
        fs::remove_file(dir.0.join("tables").join("stray.copy")).unwrap();
        fs::write(dir.0.join("blobs").join("aa").join(KEY_B), b"x").unwrap();
        assert!(
            verify(&dir.0, None)
                .unwrap_err()
                .message
                .contains("blob files")
        );
        fs::remove_file(dir.0.join("blobs").join("aa").join(KEY_B)).unwrap();
        fs::create_dir_all(dir.0.join("blobs").join("zz")).unwrap();
        assert!(
            verify(&dir.0, None)
                .unwrap_err()
                .message
                .contains("unexpected entry")
        );
        fs::remove_dir(dir.0.join("blobs").join("zz")).unwrap();
        assert!(verify(&dir.0, None).is_ok());

        fs::remove_file(dir.0.join(MANIFEST_SUM_FILE)).unwrap();
        assert!(
            verify(&dir.0, None)
                .unwrap_err()
                .message
                .contains("manifest.sha256 is missing")
        );
    }

    #[test]
    fn the_blob_list_is_bounded_and_ordered() {
        let dir = Dir::new();
        write_backup(&dir.0, &[(KEY_A, b"a"), (KEY_B, b"b")]);
        // A line without a newline or far too long never fills memory: it is refused.
        let list_path = dir.0.join(BLOB_INDEX_FILE);
        let list = fs::read(&list_path).unwrap();
        let huge = vec![b'x'; 10 * 1024 * 1024];
        fs::write(&list_path, &huge).unwrap();
        let error = for_each_blob(&dir.0, |_| Ok(())).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Format, "{error}");
        fs::write(&list_path, &list).unwrap();
        // The same list with the lines swapped is not ascending.
        let text = String::from_utf8(list).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.reverse();
        let swapped = format!("{}\n", lines.join("\n"));
        fs::write(&list_path, &swapped).unwrap();
        // Keep the manifest in step so that the order check is what fails.
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join(MANIFEST_FILE)).unwrap()).unwrap();
        manifest["blobs"]["sha256"] = serde_json::json!(sha(swapped.as_bytes()));
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(dir.0.join(MANIFEST_FILE), &bytes).unwrap();
        fs::write(
            dir.0.join(MANIFEST_SUM_FILE),
            format!("{}  {MANIFEST_FILE}\n", sha(&bytes)),
        )
        .unwrap();
        assert!(
            verify(&dir.0, None)
                .unwrap_err()
                .message
                .contains("not sorted")
        );
    }

    #[test]
    fn a_manifest_of_a_newer_server_is_refused_with_advice() {
        let dir = Dir::new();
        write_backup(&dir.0, &[]);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join(MANIFEST_FILE)).unwrap()).unwrap();
        manifest["migration_version"] = serde_json::json!(9999);
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(dir.0.join(MANIFEST_FILE), &bytes).unwrap();
        fs::write(
            dir.0.join(MANIFEST_SUM_FILE),
            format!("{}  {MANIFEST_FILE}\n", sha(&bytes)),
        )
        .unwrap();
        let error = verify(&dir.0, None).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Format);
        assert!(error.message.contains("newer one"), "{error}");
    }

    #[test]
    fn an_oversized_manifest_is_rejected_before_it_is_read() {
        let dir = Dir::new();
        write_backup(&dir.0, &[]);
        let path = dir.0.join(MANIFEST_FILE);
        fs::write(&path, vec![b' '; MAX_MANIFEST_BYTES as usize + 10]).unwrap();
        let error = verify(&dir.0, None).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Format, "{error}");
    }

    /// Links in place of the directories or top-level files are refused (unix
    /// only: creating links on Windows needs a privilege).
    #[cfg(unix)]
    #[test]
    fn links_in_place_of_directories_or_files_are_refused() {
        use std::os::unix::fs::symlink;
        for name in [
            TABLES_DIR,
            BLOBS_DIR,
            BLOB_INDEX_FILE,
            MANIFEST_FILE,
            MANIFEST_SUM_FILE,
        ] {
            let dir = Dir::new();
            write_backup(&dir.0, &[(KEY_A, b"first!")]);
            let elsewhere = Dir::new();
            fs::rename(dir.0.join(name), elsewhere.0.join(name)).unwrap();
            symlink(elsewhere.0.join(name), dir.0.join(name)).unwrap();
            let error = verify(&dir.0, None).unwrap_err();
            assert!(
                error.message.contains("real directory")
                    || error.message.contains("not a regular file"),
                "{name}: {error}"
            );
        }
    }
}
