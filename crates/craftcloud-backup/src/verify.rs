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

use crate::error::{BackupError, ErrorKind};
use crate::manifest::{
    BLOB_INDEX_FILE, BLOBS_DIR, BlobEntry, MANIFEST_FILE, MANIFEST_SUM_FILE, MAX_BLOB_LINE_BYTES,
    MAX_MANIFEST_BYTES, Manifest, TABLES_DIR, blob_file, hex, is_sha256_hex,
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
    let (mut count, mut total) = (0_u64, 0_u64);
    let mut previous: Option<String> = None;
    for_each_blob(dir, |entry| {
        if previous.as_deref().is_some_and(|p| p >= entry.key.as_str()) {
            return Err(integrity("blobs.jsonl is not sorted or repeats a key"));
        }
        previous = Some(entry.key.clone());
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

    check_no_extra_files(dir, &manifest, count)?;
    Ok(VerifyReport {
        manifest,
        manifest_sha256,
        rows,
        blobs: count,
        blob_bytes: total,
    })
}

/// Nothing may be in the directory that the manifest does not list.
fn check_no_extra_files(dir: &Path, manifest: &Manifest, blobs: u64) -> Result<(), BackupError> {
    let top: BTreeSet<&str> = [
        MANIFEST_FILE,
        MANIFEST_SUM_FILE,
        BLOB_INDEX_FILE,
        TABLES_DIR,
        BLOBS_DIR,
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
        if name == TABLES_DIR || name == BLOBS_DIR {
            seen_dirs += 1;
        }
    }
    if seen_dirs != 2 {
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

    /// Writes a complete, valid backup of empty tables and the given blobs
    /// (sorted by key by this function).
    fn write_backup(dir: &Path, blobs: &[(&str, &[u8])]) {
        let mut blobs = blobs.to_vec();
        blobs.sort_by_key(|(key, _)| *key);
        fs::create_dir_all(dir.join(TABLES_DIR)).unwrap();
        fs::create_dir_all(dir.join(BLOBS_DIR)).unwrap();
        let mut tables = Vec::new();
        for spec in backup_tables_for(13).unwrap() {
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
                sha256: sha(content),
            };
            list.extend(serde_json::to_vec(&entry).unwrap());
            list.push(b'\n');
            let path = dir.join(blob_file(key));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            total += content.len() as u64;
        }
        fs::write(dir.join(BLOB_INDEX_FILE), &list).unwrap();
        let manifest = Manifest {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
            created_at: "2026-10-29T12:00:00Z".to_owned(),
            server_version: "0.0.0".to_owned(),
            migration_version: 13,
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
            excluded_tables: Vec::new(),
            unavailable_version_blobs: 0,
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
