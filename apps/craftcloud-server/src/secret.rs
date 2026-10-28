//! Secret values and a strict loader for secret files.
//!
//! Used for the database URL (`CRAFTCLOUD_DATABASE_URL_FILE`) and the
//! bootstrap token (`CRAFTCLOUD_BOOTSTRAP_SECRET_FILE`); later milestones add
//! session secrets. Rules for a secret file: absolute path, regular file (not
//! a symlink, not a directory; on unix the open itself refuses symlinks), at
//! most 4 KiB, valid UTF-8, not empty after removing one trailing newline, and
//! on unix not readable by group or others. On non-unix platforms the
//! permission check is not performed.

use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

/// Maximum accepted size of a secret file in bytes.
pub const SECRET_FILE_MAX_BYTES: u64 = 4096;

/// A secret string. Never printed, never serialized. The stored string and
/// the read buffer are zeroed on drop; copies made by callers (for example
/// when parsing the value) are the callers' responsibility.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Access to the secret value. Callers must not log it.
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Why a secret file was rejected. Messages never contain file contents.
#[derive(Debug)]
pub enum SecretError {
    /// The path is not absolute.
    NotAbsolute(PathBuf),
    /// The file could not be inspected or read.
    Io(PathBuf, std::io::Error),
    /// The path is a symlink, directory or other non-regular file.
    NotRegularFile(PathBuf),
    /// The file is larger than [`SECRET_FILE_MAX_BYTES`].
    TooLarge(PathBuf),
    /// The file is empty after removing one trailing newline.
    Empty(PathBuf),
    /// The file is not valid UTF-8.
    NotUtf8(PathBuf),
    /// Group or others can read the file (unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    InsecurePermissions(PathBuf),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAbsolute(p) => write!(f, "secret file path {} must be absolute", p.display()),
            Self::Io(p, e) => write!(f, "cannot read secret file {}: {e}", p.display()),
            Self::NotRegularFile(p) => write!(
                f,
                "secret file {} must be a regular file (no symlink or directory)",
                p.display()
            ),
            Self::TooLarge(p) => write!(
                f,
                "secret file {} is larger than {SECRET_FILE_MAX_BYTES} bytes",
                p.display()
            ),
            Self::Empty(p) => write!(f, "secret file {} is empty", p.display()),
            Self::NotUtf8(p) => write!(f, "secret file {} is not valid UTF-8", p.display()),
            Self::InsecurePermissions(p) => write!(
                f,
                "secret file {} must not be readable by group or others (use mode 0600 or 0400)",
                p.display()
            ),
        }
    }
}

impl std::error::Error for SecretError {}

/// Loads a secret from `path`, enforcing the rules in the module docs.
pub fn load_secret_file(path: &Path) -> Result<Secret, SecretError> {
    let owned = || path.to_path_buf();
    if !path.is_absolute() {
        return Err(SecretError::NotAbsolute(owned()));
    }
    let link_meta = fs::symlink_metadata(path).map_err(|e| SecretError::Io(owned(), e))?;
    if !link_meta.file_type().is_file() {
        return Err(SecretError::NotRegularFile(owned()));
    }
    if link_meta.len() > SECRET_FILE_MAX_BYTES {
        return Err(SecretError::TooLarge(owned()));
    }

    let file = open_no_follow(path).map_err(|e| SecretError::Io(owned(), e))?;
    // Re-check on the opened handle to narrow the window for file swaps.
    let meta = file.metadata().map_err(|e| SecretError::Io(owned(), e))?;
    if !meta.is_file() {
        return Err(SecretError::NotRegularFile(owned()));
    }
    check_permissions(&meta, path)?;

    let mut buffer = Zeroizing::new(Vec::new());
    file.take(SECRET_FILE_MAX_BYTES + 1)
        .read_to_end(&mut buffer)
        .map_err(|e| SecretError::Io(owned(), e))?;
    if u64::try_from(buffer.len()).map_or(true, |n| n > SECRET_FILE_MAX_BYTES) {
        return Err(SecretError::TooLarge(owned()));
    }

    let text = std::str::from_utf8(&buffer).map_err(|_| SecretError::NotUtf8(owned()))?;
    let trimmed = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    if trimmed.is_empty() {
        return Err(SecretError::Empty(owned()));
    }
    Ok(Secret(Zeroizing::new(trimmed.to_owned())))
}

/// Opens the file read-only. On unix the open itself refuses to follow a
/// symlink (`O_NOFOLLOW`), which closes the window between the
/// `symlink_metadata` check and the open. On other platforms only the earlier
/// check and the handle re-check apply (documented limitation).
#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn check_permissions(meta: &fs::Metadata, path: &Path) -> Result<(), SecretError> {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(SecretError::InsecurePermissions(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_meta: &fs::Metadata, _path: &Path) -> Result<(), SecretError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Temp file removed on drop; avoids a `tempfile` dependency.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, content: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "craftcloud-secret-test-{}-{tag}",
                std::process::id()
            ));
            let _ = fs::remove_file(&path);
            let mut file = File::create(&path).unwrap();
            file.write_all(content).unwrap();
            drop(file);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn loads_and_trims_one_trailing_newline() {
        let f = TempFile::new("ok", b"s3cr3t-value\n");
        assert_eq!(load_secret_file(&f.0).unwrap().expose(), "s3cr3t-value");
        let f = TempFile::new("crlf", b"abc\r\n");
        assert_eq!(load_secret_file(&f.0).unwrap().expose(), "abc");
        let f = TempFile::new("two", b"abc\n\n");
        assert_eq!(load_secret_file(&f.0).unwrap().expose(), "abc\n");
        let f = TempFile::new("none", b"abc");
        assert_eq!(load_secret_file(&f.0).unwrap().expose(), "abc");
    }

    #[test]
    fn debug_and_display_are_redacted() {
        let f = TempFile::new("redact", b"topsecret");
        let secret = load_secret_file(&f.0).unwrap();
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(format!("{secret}"), "[redacted]");
        assert!(!format!("{:?}", Some(&secret)).contains("topsecret"));
    }

    #[test]
    fn rejects_relative_path() {
        let err = load_secret_file(Path::new("relative/secret")).unwrap_err();
        assert!(matches!(err, SecretError::NotAbsolute(_)));
    }

    #[test]
    fn rejects_missing_file_and_directory() {
        let missing = std::env::temp_dir().join("craftcloud-secret-test-does-not-exist");
        assert!(matches!(
            load_secret_file(&missing).unwrap_err(),
            SecretError::Io(..)
        ));
        assert!(matches!(
            load_secret_file(&std::env::temp_dir()).unwrap_err(),
            SecretError::NotRegularFile(_)
        ));
    }

    #[test]
    fn rejects_empty_and_newline_only() {
        for (tag, content) in [("empty", &b""[..]), ("nl", b"\n"), ("crlf-only", b"\r\n")] {
            let f = TempFile::new(tag, content);
            assert!(matches!(
                load_secret_file(&f.0).unwrap_err(),
                SecretError::Empty(_)
            ));
        }
    }

    #[test]
    fn size_bound_is_exact() {
        let f = TempFile::new("max", &vec![b'a'; 4096]);
        assert!(load_secret_file(&f.0).is_ok());
        let f = TempFile::new("over", &vec![b'a'; 4097]);
        assert!(matches!(
            load_secret_file(&f.0).unwrap_err(),
            SecretError::TooLarge(_)
        ));
    }

    #[test]
    fn rejects_non_utf8_without_leaking_content() {
        let f = TempFile::new("utf8", &[0x73, 0x65, 0xff, 0xfe]);
        let err = load_secret_file(&f.0).unwrap_err();
        assert!(matches!(err, SecretError::NotUtf8(_)));
    }

    #[test]
    fn error_messages_do_not_contain_secret_content() {
        let content = "hunter2-hunter2";
        let f = TempFile::new("leak", format!("{content}{}", "x".repeat(5000)).as_bytes());
        let err = load_secret_file(&f.0).unwrap_err();
        assert!(!err.to_string().contains(content));
        assert!(!format!("{err:?}").contains(content));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_or_other_readable() {
        use std::os::unix::fs::PermissionsExt;
        let f = TempFile::new("perm", b"secret");
        for mode in [0o640, 0o604, 0o644, 0o660] {
            fs::set_permissions(&f.0, fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(
                load_secret_file(&f.0).unwrap_err(),
                SecretError::InsecurePermissions(_)
            ));
        }
        fs::set_permissions(&f.0, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(load_secret_file(&f.0).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink() {
        let target = TempFile::new("link-target", b"secret");
        let link = std::env::temp_dir().join(format!(
            "craftcloud-secret-test-{}-link",
            std::process::id()
        ));
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&target.0, &link).unwrap();
        let result = load_secret_file(&link);
        let _ = fs::remove_file(&link);
        assert!(matches!(
            result.unwrap_err(),
            SecretError::NotRegularFile(_)
        ));
    }
}
