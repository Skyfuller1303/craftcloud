//! The manifest of a backup directory and its bounds.
//!
//! Layout of a backup directory (all names are fixed or derived from
//! validated values, never from free text):
//!
//! ```text
//! manifest.json          this file's content, see [`Manifest`]
//! manifest.sha256        "<64 hex>  manifest.json"
//! blobs.jsonl            one [`BlobEntry`] per line, ascending by key
//! tables/<table>.copy    PostgreSQL COPY text format of one table
//! blobs/<xx>/<key>       the object bytes (xx = first two characters of the key)
//! ```

use serde::{Deserialize, Serialize};

use crate::error::{BackupError, ErrorKind};

/// Value of [`Manifest::format`].
pub const FORMAT: &str = "craftcloud-backup";
/// The only format version this build reads and writes.
pub const FORMAT_VERSION: u32 = 1;

/// File name of the manifest.
pub const MANIFEST_FILE: &str = "manifest.json";
/// File name of the manifest checksum.
pub const MANIFEST_SUM_FILE: &str = "manifest.sha256";
/// File name of the blob list.
pub const BLOB_INDEX_FILE: &str = "blobs.jsonl";
/// Directory of the table files.
pub const TABLES_DIR: &str = "tables";
/// Directory of the blob files.
pub const BLOBS_DIR: &str = "blobs";

/// Largest accepted manifest (it has one entry per table, so it is tiny).
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Largest accepted line of `blobs.jsonl`.
pub const MAX_BLOB_LINE_BYTES: usize = 1024;
/// Most tables a manifest may list.
pub const MAX_TABLES: usize = 64;

/// Identity of the store the backup was taken from (information only; a
/// restore creates a new identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreInfo {
    /// The store identity from the marker file.
    pub store_id: String,
    /// The binding epoch of the database at backup time.
    pub epoch: i64,
}

/// One table file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableEntry {
    /// Table name.
    pub name: String,
    /// Relative path (always `tables/<name>.copy`).
    pub file: String,
    /// Number of rows.
    pub rows: u64,
    /// Size of the file in bytes.
    pub bytes: u64,
    /// SHA-256 of the file, lowercase hex.
    pub sha256: String,
}

/// The blob list file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobIndex {
    /// Always `blobs.jsonl`.
    pub file: String,
    /// Number of blobs (lines).
    pub count: u64,
    /// Sum of the blob sizes.
    pub total_bytes: u64,
    /// SHA-256 of the list file, lowercase hex.
    pub sha256: String,
}

/// One line of `blobs.jsonl`: a committed blob and its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobEntry {
    /// The `blobs.id` of the database row.
    pub id: String,
    /// The object key (32 lowercase hex characters).
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the content, lowercase hex.
    pub sha256: String,
}

/// The manifest. Contains no secret: no database URL, no password, no token,
/// no names or e-mail addresses of users.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Always [`FORMAT`].
    pub format: String,
    /// Always [`FORMAT_VERSION`] for this build.
    pub format_version: u32,
    /// The time of the database snapshot (RFC 3339, UTC): the recovery point.
    pub created_at: String,
    /// `CARGO_PKG_VERSION` of the server that made the backup.
    pub server_version: String,
    /// The newest applied migration at snapshot time.
    pub migration_version: i64,
    /// The store the backup was taken from.
    pub store: StoreInfo,
    /// One entry per table of `BACKUP_TABLES`, in restore order.
    pub tables: Vec<TableEntry>,
    /// The blob list.
    pub blobs: BlobIndex,
    /// Tables left out on purpose (see `EXCLUDED_TABLES`).
    pub excluded_tables: Vec<String>,
    /// Versions whose blob was not `committed` at snapshot time (for example
    /// `corrupt`): their bytes are not in this backup.
    pub unavailable_version_blobs: u64,
}

/// Lowercase hex of a byte slice.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Parses 64 lowercase hex characters into a digest.
#[must_use]
pub fn parse_sha256(text: &str) -> Option<[u8; 32]> {
    if !is_sha256_hex(text) {
        return None;
    }
    let mut out = [0_u8; 32];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let high = nibble(*pair.first()?)?;
        let low = nibble(*pair.get(1)?)?;
        *slot = (high << 4) | low;
    }
    Some(out)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// 64 lowercase hex characters.
#[must_use]
pub fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 32 lowercase hex characters.
#[must_use]
pub fn is_key(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Relative path of a table file.
#[must_use]
pub fn table_file(name: &str) -> String {
    format!("{TABLES_DIR}/{name}.copy")
}

/// Relative path of a blob file for a validated key.
#[must_use]
pub fn blob_file(key: &str) -> String {
    format!("{BLOBS_DIR}/{}/{key}", key.get(..2).unwrap_or("00"))
}

fn format_error(message: impl Into<String>) -> BackupError {
    BackupError::new(ErrorKind::Format, message)
}

impl Manifest {
    /// Parses and checks the manifest text. `known_migrations` are the
    /// migration versions of this build.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Format`] for anything that is not exactly a manifest of
    /// format version 1 with consistent entries.
    pub fn parse(bytes: &[u8], known_migrations: &[i64]) -> Result<Self, BackupError> {
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
            return Err(format_error("the manifest is too large"));
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|_| format_error("the manifest is not valid JSON of a CraftCloud backup"))?;
        manifest.check(known_migrations)?;
        Ok(manifest)
    }

    fn check(&self, known_migrations: &[i64]) -> Result<(), BackupError> {
        if self.format != FORMAT {
            return Err(format_error("this directory is not a CraftCloud backup"));
        }
        if self.format_version != FORMAT_VERSION {
            return Err(format_error(format!(
                "unsupported backup format version {} (this build reads version {FORMAT_VERSION})",
                self.format_version
            )));
        }
        if !known_migrations.contains(&self.migration_version) {
            let newest = known_migrations.iter().max().copied().unwrap_or(0);
            return Err(format_error(if self.migration_version > newest {
                format!(
                    "the backup has migration version {} but this server only knows up to {newest}; \
                     restore it with the server version that made it, or a newer one",
                    self.migration_version
                )
            } else {
                format!(
                    "the backup has migration version {}, which this server does not know",
                    self.migration_version
                )
            }));
        }
        if self.tables.len() > MAX_TABLES {
            return Err(format_error("the manifest lists too many tables"));
        }
        let expected: Vec<&str> = crate::tables::backup_tables_for(self.migration_version)?
            .iter()
            .map(|t| t.name)
            .collect();
        let listed: Vec<&str> = self.tables.iter().map(|t| t.name.as_str()).collect();
        if expected != listed {
            return Err(format_error(
                "the table list of the manifest does not match this server's backup tables",
            ));
        }
        for table in &self.tables {
            if table.file != table_file(&table.name) || !is_sha256_hex(&table.sha256) {
                return Err(format_error(format!(
                    "the manifest entry of table {} is malformed",
                    table.name
                )));
            }
        }
        if self.blobs.file != BLOB_INDEX_FILE || !is_sha256_hex(&self.blobs.sha256) {
            return Err(format_error(
                "the blob list entry of the manifest is malformed",
            ));
        }
        if self.store.store_id.len() > 64 {
            return Err(format_error("the manifest holds an oversized field"));
        }
        if self.server_version.is_empty()
            || self.server_version.len() > 64
            || !self
                .server_version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
        {
            return Err(format_error("the manifest has an invalid server version"));
        }
        if time::OffsetDateTime::parse(
            &self.created_at,
            &time::format_description::well_known::Rfc3339,
        )
        .is_err()
        {
            return Err(format_error("the manifest has no valid creation time"));
        }
        Ok(())
    }

    /// Serializes to the pretty JSON that is written to disk.
    ///
    /// # Errors
    ///
    /// Only if serialization fails, which it cannot for these types.
    pub fn to_bytes(&self) -> Result<Vec<u8>, BackupError> {
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|_| format_error("the manifest cannot be serialized"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

impl BlobEntry {
    /// Parses one line of `blobs.jsonl` and checks every field.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Format`] for a malformed line.
    pub fn parse_line(line: &[u8]) -> Result<Self, BackupError> {
        if line.len() > MAX_BLOB_LINE_BYTES {
            return Err(format_error("a line of the blob list is too long"));
        }
        let entry: Self = serde_json::from_slice(line)
            .map_err(|_| format_error("a line of the blob list is malformed"))?;
        if !is_key(&entry.key)
            || !is_sha256_hex(&entry.sha256)
            || uuid::Uuid::parse_str(&entry.id).is_err()
        {
            return Err(format_error("a line of the blob list has an invalid field"));
        }
        Ok(entry)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
            created_at: "2026-10-29T12:00:00Z".to_owned(),
            server_version: "0.0.0".to_owned(),
            migration_version: 14,
            store: StoreInfo {
                store_id: "0b2f6b5e-6d7b-4f5e-9d0a-3a5b3c1d2e4f".to_owned(),
                epoch: 0,
            },
            tables: crate::tables::backup_tables_for(14)
                .unwrap()
                .iter()
                .map(|t| TableEntry {
                    name: t.name.to_owned(),
                    file: table_file(t.name),
                    rows: 0,
                    bytes: 0,
                    sha256: "0".repeat(64),
                })
                .collect(),
            blobs: BlobIndex {
                file: BLOB_INDEX_FILE.to_owned(),
                count: 0,
                total_bytes: 0,
                sha256: "0".repeat(64),
            },
            excluded_tables: vec!["sessions".to_owned()],
            unavailable_version_blobs: 0,
        }
    }

    const KNOWN: &[i64] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];

    fn parse(manifest: &Manifest) -> Result<Manifest, BackupError> {
        Manifest::parse(&manifest.to_bytes().unwrap(), KNOWN)
    }

    #[test]
    fn a_manifest_round_trips() {
        assert_eq!(parse(&sample()).unwrap(), sample());
    }

    #[test]
    fn unknown_format_and_versions_are_rejected_with_a_reason() {
        let mut m = sample();
        m.format = "other".to_owned();
        assert!(
            parse(&m)
                .unwrap_err()
                .message
                .contains("not a CraftCloud backup")
        );
        let mut m = sample();
        m.format_version = 2;
        assert!(parse(&m).unwrap_err().message.contains("format version 2"));
        let mut m = sample();
        m.migration_version = 15;
        let error = parse(&m).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Format);
        assert!(error.message.contains("only knows up to 14"), "{error}");
        let mut m = sample();
        m.migration_version = 0;
        assert!(parse(&m).unwrap_err().message.contains("does not know"));
        // Known to the build but too old to have an audit log.
        let mut m = sample();
        m.migration_version = 10;
        assert!(parse(&m).unwrap_err().message.contains("backup support"));
    }

    #[test]
    fn extra_missing_or_renamed_entries_are_rejected() {
        let mut m = sample();
        m.tables.pop();
        assert!(parse(&m).is_err());
        let mut m = sample();
        m.tables.swap(0, 1);
        assert!(parse(&m).is_err());
        let mut m = sample();
        m.tables[0].file = "tables/../../etc/passwd".to_owned();
        assert!(parse(&m).is_err());
        let mut m = sample();
        m.tables[0].sha256 = "ABC".to_owned();
        assert!(parse(&m).is_err());
        let mut m = sample();
        m.blobs.file = "../blobs.jsonl".to_owned();
        assert!(parse(&m).is_err());
        let mut m = sample();
        m.created_at = "yesterday".to_owned();
        assert!(parse(&m).is_err());
        for bad in [
            "",
            "1.0 beta",
            "../x",
            "a
b",
            &"1".repeat(65),
        ] {
            let mut m = sample();
            m.server_version = bad.to_owned();
            assert!(parse(&m).is_err(), "{bad:?}");
        }
        let mut m = sample();
        m.server_version = "1.2.3-rc.1+build5".to_owned();
        assert!(parse(&m).is_ok());
    }

    #[test]
    fn unknown_fields_garbage_and_oversize_are_rejected() {
        let text = String::from_utf8(sample().to_bytes().unwrap()).unwrap();
        let extra = text.replacen('{', "{\"surprise\": 1,", 1);
        assert!(Manifest::parse(extra.as_bytes(), KNOWN).is_err());
        assert!(Manifest::parse(b"not json", KNOWN).is_err());
        assert!(Manifest::parse(b"", KNOWN).is_err());
        let huge = vec![b' '; usize::try_from(MAX_MANIFEST_BYTES).unwrap() + 1];
        assert!(
            Manifest::parse(&huge, KNOWN)
                .unwrap_err()
                .message
                .contains("too large")
        );
    }

    #[test]
    fn blob_lines_are_validated() {
        let ok = format!(
            "{{\"id\":\"{}\",\"key\":\"{}\",\"size\":3,\"sha256\":\"{}\"}}",
            uuid::Uuid::nil(),
            "a".repeat(32),
            "b".repeat(64)
        );
        assert_eq!(BlobEntry::parse_line(ok.as_bytes()).unwrap().size, 3);
        for bad in [
            ok.replace(&"a".repeat(32), "../../etc/passwd"),
            ok.replace(&"a".repeat(32), &"A".repeat(32)),
            ok.replace(&"b".repeat(64), "00"),
            ok.replace("\"size\":3", "\"size\":-3"),
            ok.replace(&uuid::Uuid::nil().to_string(), "x"),
            format!("{ok}{}", " ".repeat(MAX_BLOB_LINE_BYTES)),
        ] {
            assert!(BlobEntry::parse_line(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn hex_helpers_round_trip() {
        let digest = [0xAB_u8; 32];
        let text = hex(&digest);
        assert_eq!(text.len(), 64);
        assert_eq!(parse_sha256(&text), Some(digest));
        assert_eq!(parse_sha256("zz"), None);
        assert_eq!(
            blob_file(&"c".repeat(32)),
            format!("blobs/cc/{}", "c".repeat(32))
        );
    }
}
