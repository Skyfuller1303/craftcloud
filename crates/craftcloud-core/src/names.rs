//! Names of projects and files and the identifiers of files (app, format).
//!
//! A file name is untrusted display metadata. It is validated so that it is
//! safe to show and to hand to a desktop application, but it is never used to
//! build a storage path (storage keys are ids). The same Unicode rules decide
//! whether two names collide: [`name_key`] folds NFC normalization and case.

use std::fmt;

use unicode_normalization::UnicodeNormalization;

use crate::identity::{DisplayName, DisplayNameError, is_format_char, is_visible_char};

/// Maximum length of a file name in UTF-8 bytes (after NFC normalization).
pub const FILE_NAME_MAX_BYTES: usize = 255;
/// Maximum length of a file `app` identifier.
pub const FILE_APP_MAX_LEN: usize = 32;
/// Maximum length of a file `format` identifier.
pub const FILE_FORMAT_MAX_LEN: usize = 64;

/// Characters that are never valid in a file name: path separators and the
/// characters reserved by common file systems.
const FORBIDDEN_CHARS: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Names that Windows treats as devices, with or without an extension.
const RESERVED_STEMS: [&str; 7] = ["con", "prn", "aux", "nul", "conin$", "conout$", "clock$"];

/// NFC-normalizes `raw`.
fn nfc(raw: &str) -> String {
    raw.nfc().collect()
}

/// The comparison key of a name: NFKC (compatibility folding, so fullwidth
/// letters and ligatures such as U+FB03 collide with their plain spelling),
/// case-folded (upper then lower case, so that `ß` and `SS` collide), NFKC
/// again. Two names with the same key count as the same name within a project
/// or workspace. The displayed name keeps its NFC form. The key can be longer
/// than the name (NFKC expands e.g. U+FDFA to 18 characters); the database
/// allows for that.
#[must_use]
pub fn name_key(name: &str) -> String {
    let folded: String = name
        .nfkc()
        .collect::<String>()
        .to_uppercase()
        .to_lowercase();
    folded.nfkc().collect()
}

/// Why a file name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileNameError {
    /// Empty.
    Empty,
    /// Longer than [`FILE_NAME_MAX_BYTES`] bytes.
    TooLong,
    /// Contains a control character (including NUL).
    ControlCharacter,
    /// Contains a path separator or a character reserved by file systems.
    ForbiddenCharacter,
    /// Contains an invisible format character (bidirectional controls,
    /// zero-width characters) or unusual whitespace.
    FormatCharacter,
    /// Is `.` or `..`.
    DotName,
    /// Starts or ends with whitespace.
    EdgeWhitespace,
    /// Ends with a dot.
    TrailingDot,
    /// Is a reserved Windows device name (with or without extension).
    ReservedName,
    /// Has no visible character.
    NoVisibleCharacter,
}

impl fmt::Display for FileNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "file name must not be empty",
            Self::TooLong => "file name is too long",
            Self::ControlCharacter => "file name contains control characters",
            Self::ForbiddenCharacter => "file name contains a forbidden character",
            Self::FormatCharacter => "file name contains invisible or unusual characters",
            Self::DotName => "file name must not be . or ..",
            Self::EdgeWhitespace => "file name must not start or end with whitespace",
            Self::TrailingDot => "file name must not end with a dot",
            Self::ReservedName => "file name is a reserved device name",
            Self::NoVisibleCharacter => "file name must contain a visible character",
        })
    }
}

impl std::error::Error for FileNameError {}

/// A validated, NFC-normalized file name of 1 to 255 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName(String);

impl FileName {
    /// Parses `raw`: NFC-normalizes it and checks every rule. Nothing is
    /// trimmed or repaired; unsuitable names are rejected.
    ///
    /// # Errors
    ///
    /// A [`FileNameError`] describing the first violated rule.
    pub fn parse(raw: &str) -> Result<Self, FileNameError> {
        // Bound the work before normalizing: NFC can shrink or grow a string
        // by a small factor only, 4x the limit is a safe upper bound.
        if raw.len() > FILE_NAME_MAX_BYTES * 4 {
            return Err(FileNameError::TooLong);
        }
        let name = nfc(raw);
        if name.is_empty() {
            return Err(FileNameError::Empty);
        }
        if name.len() > FILE_NAME_MAX_BYTES {
            return Err(FileNameError::TooLong);
        }
        if name.chars().any(char::is_control) {
            return Err(FileNameError::ControlCharacter);
        }
        if name.chars().any(|c| FORBIDDEN_CHARS.contains(&c)) {
            return Err(FileNameError::ForbiddenCharacter);
        }
        // Only the plain space is accepted as whitespace inside a name.
        if name
            .chars()
            .any(|c| is_format_char(c) || (c.is_whitespace() && c != ' '))
        {
            return Err(FileNameError::FormatCharacter);
        }
        if name.starts_with(' ') || name.ends_with(' ') {
            return Err(FileNameError::EdgeWhitespace);
        }
        if name == "." || name == ".." {
            return Err(FileNameError::DotName);
        }
        if name.ends_with('.') {
            return Err(FileNameError::TrailingDot);
        }
        if !name.chars().any(is_visible_char) {
            return Err(FileNameError::NoVisibleCharacter);
        }
        if is_reserved_device_name(&name) {
            return Err(FileNameError::ReservedName);
        }
        Ok(Self(name))
    }

    /// The validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The collision key (see [`name_key`]).
    #[must_use]
    pub fn key(&self) -> String {
        name_key(&self.0)
    }
}

/// Windows device names: the part before the first dot decides (`con.txt` and
/// `CON.tar.gz` are devices too), trailing spaces before the dot are ignored.
fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let lower = stem.to_lowercase();
    if RESERVED_STEMS.contains(&lower.as_str()) {
        return true;
    }
    // COM0-9 and LPT0-9, also with the superscript digits Windows accepts.
    for prefix in ["com", "lpt"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let mut chars = rest.chars();
            if let (Some(digit), None) = (chars.next(), chars.next())
                && matches!(digit, '0'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}')
            {
                return true;
            }
        }
    }
    false
}

/// A project name: the [`DisplayName`] rules after NFC normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectName(String);

impl ProjectName {
    /// Parses `raw`.
    ///
    /// # Errors
    ///
    /// A [`DisplayNameError`] describing the first violated rule.
    pub fn parse(raw: &str) -> Result<Self, DisplayNameError> {
        if raw.len() > crate::DISPLAY_NAME_MAX_CHARS * 16 {
            return Err(DisplayNameError::TooLong);
        }
        let name = DisplayName::parse(&nfc(raw))?;
        Ok(Self(name.as_str().to_owned()))
    }

    /// The validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The collision key (see [`name_key`]).
    #[must_use]
    pub fn key(&self) -> String {
        name_key(&self.0)
    }
}

/// Why an app or format identifier was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentifierError;

impl fmt::Display for IdentifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("identifier is empty, too long or contains forbidden characters")
    }
}

impl std::error::Error for IdentifierError {}

fn identifier(raw: &str, max: usize, extra: &[char]) -> Result<String, IdentifierError> {
    let valid = !raw.is_empty()
        && raw.len() <= max
        && raw.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || extra.contains(&c)
        });
    if valid {
        Ok(raw.to_owned())
    } else {
        Err(IdentifierError)
    }
}

/// The application a file belongs to, matching `^[a-z0-9-]{1,32}$`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileApp(String);

impl FileApp {
    /// Parses `raw`.
    ///
    /// # Errors
    ///
    /// [`IdentifierError`] if `raw` does not match the pattern.
    pub fn parse(raw: &str) -> Result<Self, IdentifierError> {
        identifier(raw, FILE_APP_MAX_LEN, &[]).map(Self)
    }

    /// The identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The format identifier of a file, matching `^[a-z0-9.+-]{1,64}$`. It is a
/// label only; no behavior depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFormat(String);

impl FileFormat {
    /// Suggests the registry application for this validated format identifier.
    /// Unknown formats retain support through the generic application.
    #[must_use]
    pub fn registered_app(&self) -> &'static str {
        crate::formats::app_for_extension(self.as_str())
    }

    /// Parses `raw`.
    ///
    /// # Errors
    ///
    /// [`IdentifierError`] if `raw` does not match the pattern.
    pub fn parse(raw: &str) -> Result<Self, IdentifierError> {
        identifier(raw, FILE_FORMAT_MAX_LEN, &['.', '+']).map(Self)
    }

    /// The identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> String {
        FileName::parse(raw)
            .unwrap_or_else(|e| panic!("{raw:?} rejected: {e}"))
            .as_str()
            .to_owned()
    }

    fn err(raw: &str) -> FileNameError {
        FileName::parse(raw).expect_err(raw)
    }

    #[test]
    fn ordinary_names_are_accepted_unchanged() {
        for name in [
            "logo.fig",
            "Design v2 (final).figcraft",
            "日本語のファイル.png",
            "emoji 🎨.txt",
            ".hidden",
            "a",
            "file.tar.gz",
            "console.txt",
            "COM10.txt",
            "comx",
        ] {
            assert_eq!(ok(name), name);
        }
    }

    #[test]
    fn nfc_and_nfd_spellings_are_the_same_name() {
        let composed = "caf\u{00E9}.txt";
        let decomposed = "cafe\u{0301}.txt";
        assert_ne!(composed, decomposed);
        assert_eq!(ok(decomposed), composed);
        assert_eq!(
            FileName::parse(decomposed).map(|n| n.key()),
            FileName::parse(composed).map(|n| n.key())
        );
    }

    #[test]
    fn names_collide_regardless_of_case_and_normalization() {
        let key = |s: &str| FileName::parse(s).map(|n| n.key()).unwrap_or_default();
        assert_eq!(key("Logo.PNG"), key("logo.png"));
        assert_eq!(key("Stra\u{00DF}e.txt"), key("STRASSE.txt"));
        assert_eq!(key("CAFE\u{0301}"), key("caf\u{00E9}"));
        assert_ne!(key("a.txt"), key("b.txt"));
        // Compatibility look-alikes collide: fullwidth letters and ligatures.
        assert_eq!(key("Report.psd"), key("\u{FF32}eport.psd"));
        assert_eq!(key("office"), key("o\u{FB03}ce"));
        assert_eq!(key("\u{FF21}\u{FF22}"), key("ab"));
        assert_ne!(key("logo.png"), key("logo.png "));
    }

    #[test]
    fn rejected_names_by_rule() {
        let long = "x".repeat(256);
        let table: [(&str, FileNameError); 31] = [
            ("", FileNameError::Empty),
            (long.as_str(), FileNameError::TooLong),
            ("a\0b", FileNameError::ControlCharacter),
            ("\0", FileNameError::ControlCharacter),
            ("a\tb", FileNameError::ControlCharacter),
            ("a\nb", FileNameError::ControlCharacter),
            ("a\u{007F}b", FileNameError::ControlCharacter),
            ("a\u{0085}b", FileNameError::ControlCharacter),
            ("../etc/passwd", FileNameError::ForbiddenCharacter),
            ("..\\win", FileNameError::ForbiddenCharacter),
            ("a/b", FileNameError::ForbiddenCharacter),
            ("a:b", FileNameError::ForbiddenCharacter),
            ("a*b", FileNameError::ForbiddenCharacter),
            ("a?b", FileNameError::ForbiddenCharacter),
            ("a\"b", FileNameError::ForbiddenCharacter),
            ("a<b", FileNameError::ForbiddenCharacter),
            ("a|b", FileNameError::ForbiddenCharacter),
            ("a\u{202E}gnp.exe", FileNameError::FormatCharacter),
            ("a\u{200B}b", FileNameError::FormatCharacter),
            ("a\u{00A0}b", FileNameError::FormatCharacter),
            ("a\u{2028}b", FileNameError::FormatCharacter),
            (" a", FileNameError::EdgeWhitespace),
            ("a ", FileNameError::EdgeWhitespace),
            (".", FileNameError::DotName),
            ("..", FileNameError::DotName),
            ("a.", FileNameError::TrailingDot),
            ("CON", FileNameError::ReservedName),
            ("nul.txt", FileNameError::ReservedName),
            ("Com1.tar.gz", FileNameError::ReservedName),
            ("LPT9.x", FileNameError::ReservedName),
            ("aux ", FileNameError::EdgeWhitespace),
        ];
        for (raw, expected) in table {
            assert_eq!(err(raw), expected, "{raw:?}");
        }
        for reserved in [
            "CONIN$",
            "conout$.txt",
            "Clock$",
            "COM0",
            "lpt0.log",
            "LPT0",
        ] {
            assert_eq!(err(reserved), FileNameError::ReservedName, "{reserved}");
        }
        // Trailing space before the extension does not hide a device name.
        assert_eq!(err("CON .txt"), FileNameError::ReservedName);
    }

    #[test]
    fn length_counts_bytes_after_normalization() {
        assert_eq!(ok(&"x".repeat(255)).len(), 255);
        // 128 two-byte characters are 256 bytes.
        assert_eq!(err(&"\u{00E9}".repeat(128)), FileNameError::TooLong);
        assert_eq!(ok(&"\u{00E9}".repeat(127)).len(), 254);
        // Huge input is refused without normalizing it.
        assert_eq!(err(&"a".repeat(1_000_000)), FileNameError::TooLong);
        // NFD input that shrinks below the limit after composition is fine.
        let decomposed = "e\u{0301}".repeat(100);
        assert_eq!(ok(&decomposed).len(), 200);
    }

    #[test]
    fn keys_of_maximal_expansion_stay_bounded() {
        // U+FDFA expands to 18 characters; 85 of them fit into 255 bytes.
        let name = "\u{FDFA}".repeat(85);
        let file = FileName::parse(&name).unwrap_or_else(|e| panic!("{e}"));
        assert!(file.key().len() <= 8192, "{}", file.key().len());
        let project = ProjectName::parse(&"\u{FDFA}".repeat(100)).unwrap_or_else(|e| panic!("{e}"));
        assert!(project.key().chars().count() <= 4096);
    }

    #[test]
    fn invisible_only_names_are_rejected() {
        assert_eq!(err("\u{3164}"), FileNameError::NoVisibleCharacter);
        assert_eq!(err("\u{2800}"), FileNameError::NoVisibleCharacter);
    }

    #[test]
    fn project_names_follow_display_rules_with_nfc() {
        assert_eq!(
            ProjectName::parse("  Caf\u{0065}\u{0301}  ").map(|n| n.as_str().to_owned()),
            Ok("caf\u{00E9}".replace('c', "C"))
        );
        assert!(ProjectName::parse("").is_err());
        assert!(ProjectName::parse("a\u{202E}b").is_err());
        assert!(ProjectName::parse("a\0b").is_err());
        assert!(ProjectName::parse(&"x".repeat(101)).is_err());
        assert_eq!(
            ProjectName::parse("Alpha").map(|n| n.key()),
            ProjectName::parse("ALPHA").map(|n| n.key())
        );
    }

    #[test]
    fn app_and_format_identifiers() {
        for good in ["figcraft", "a", "photo-craft2", &"x".repeat(32)] {
            assert!(FileApp::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "Figcraft", "a b", "a.b", "a_b", "é", &"x".repeat(33)] {
            assert!(FileApp::parse(bad).is_err(), "{bad}");
        }
        for good in [
            "fig",
            "image/png".replace('/', "-").as_str(),
            "svg+xml",
            "tar.gz",
            &"x".repeat(64),
        ] {
            assert!(FileFormat::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "PNG", "a/b", "a b", "a_b", &"x".repeat(65)] {
            assert!(FileFormat::parse(bad).is_err(), "{bad}");
        }
    }
}
