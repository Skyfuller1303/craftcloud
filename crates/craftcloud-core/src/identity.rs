//! Validated identity values: e-mail addresses, display names and slugs.
//!
//! All parsers are pure, never panic, and never include the rejected input
//! in their error values or messages.

use std::fmt;

/// Maximum length of a normalized e-mail address in characters.
pub const EMAIL_MAX_CHARS: usize = 320;
/// Maximum length of a display name in characters.
pub const DISPLAY_NAME_MAX_CHARS: usize = 100;
/// Maximum length of a slug in characters.
pub const SLUG_MAX_CHARS: usize = 63;

/// Why an e-mail address was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailError {
    /// Empty after trimming.
    Empty,
    /// Longer than [`EMAIL_MAX_CHARS`] after normalization.
    TooLong,
    /// Contains whitespace or control characters.
    ForbiddenCharacter,
    /// Not exactly one `@`.
    AtSignCount,
    /// Local part or domain is empty.
    EmptyPart,
}

impl fmt::Display for EmailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "e-mail address must not be empty",
            Self::TooLong => "e-mail address is too long",
            Self::ForbiddenCharacter => "e-mail address contains whitespace or control characters",
            Self::AtSignCount => "e-mail address must contain exactly one @",
            Self::EmptyPart => "e-mail address needs a local part and a domain",
        })
    }
}

impl std::error::Error for EmailError {}

/// A trimmed, lowercased e-mail address with basic structural validation.
///
/// This is not full RFC 5322 validation: it guarantees what the database
/// relies on (lowercase, bounded, one `@`, no whitespace or control
/// characters). Unicode is lowercased with `str::to_lowercase`; no Unicode
/// normalization form is applied.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NormalizedEmail(String);

impl NormalizedEmail {
    /// Parses and normalizes `raw`.
    ///
    /// # Errors
    ///
    /// An [`EmailError`] describing the first violated rule.
    pub fn parse(raw: &str) -> Result<Self, EmailError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(EmailError::Empty);
        }
        // Bound the work before lowercasing (lowercasing can grow a string).
        if trimmed.chars().take(EMAIL_MAX_CHARS * 2 + 1).count() > EMAIL_MAX_CHARS * 2 {
            return Err(EmailError::TooLong);
        }
        if trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(EmailError::ForbiddenCharacter);
        }
        let lowered = trimmed.to_lowercase();
        if lowered.chars().count() > EMAIL_MAX_CHARS {
            return Err(EmailError::TooLong);
        }
        if lowered.matches('@').count() != 1 {
            return Err(EmailError::AtSignCount);
        }
        match lowered.split_once('@') {
            Some((local, domain)) if !local.is_empty() && !domain.is_empty() => Ok(Self(lowered)),
            _ => Err(EmailError::EmptyPart),
        }
    }

    /// The normalized address.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The part before the `@`.
    #[must_use]
    pub fn local_part(&self) -> &str {
        self.0.split_once('@').map_or(self.0.as_str(), |(l, _)| l)
    }
}

impl fmt::Display for NormalizedEmail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a display name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayNameError {
    /// Empty after trimming.
    Empty,
    /// Longer than [`DISPLAY_NAME_MAX_CHARS`].
    TooLong,
    /// Contains control characters.
    ControlCharacter,
    /// Contains invisible format characters (Unicode category Cf), for
    /// example bidirectional overrides or zero-width characters.
    FormatCharacter,
    /// Contains no visible character (only whitespace or invisible ones).
    NoVisibleCharacter,
}

impl fmt::Display for DisplayNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "name must not be empty",
            Self::TooLong => "name is too long",
            Self::ControlCharacter => "name contains control characters",
            Self::FormatCharacter => "name contains invisible formatting characters",
            Self::NoVisibleCharacter => "name must contain at least one visible character",
        })
    }
}

impl std::error::Error for DisplayNameError {}

/// A trimmed display name of 1 to 100 characters without control characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayName(String);

impl DisplayName {
    /// Parses `raw`.
    ///
    /// # Errors
    ///
    /// A [`DisplayNameError`] describing the first violated rule.
    pub fn parse(raw: &str) -> Result<Self, DisplayNameError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DisplayNameError::Empty);
        }
        if trimmed.chars().take(DISPLAY_NAME_MAX_CHARS + 1).count() > DISPLAY_NAME_MAX_CHARS {
            return Err(DisplayNameError::TooLong);
        }
        if trimmed.chars().any(char::is_control) {
            return Err(DisplayNameError::ControlCharacter);
        }
        if trimmed.chars().any(is_format_char) {
            return Err(DisplayNameError::FormatCharacter);
        }
        if !trimmed.chars().any(is_visible_char) {
            return Err(DisplayNameError::NoVisibleCharacter);
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// The validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Unicode General_Category Cf (format) characters, by explicit ranges (no
/// Unicode data crate is used). Covers bidirectional controls and overrides
/// (U+200E/200F, U+202A-202E, U+2066-2069, U+061C), zero-width characters
/// (U+200B-200D, U+2060-2064, U+FEFF) and the other Cf code points of
/// Unicode 15; newer additions are not covered.
fn is_format_char(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// A character that renders as something: not whitespace, not a format
/// character and not one of the common blank "filler" letters.
fn is_visible_char(c: char) -> bool {
    !c.is_whitespace()
        && !is_format_char(c)
        && !matches!(u32::from(c), 0x115F | 0x1160 | 0x2800 | 0x3164 | 0xFFA0)
}

/// Why a slug was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlugError {
    /// Empty.
    Empty,
    /// Longer than [`SLUG_MAX_CHARS`].
    TooLong,
    /// Does not match `^[a-z0-9][a-z0-9-]{0,62}$`.
    InvalidFormat,
}

impl fmt::Display for SlugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "slug must not be empty",
            Self::TooLong => "slug is too long",
            Self::InvalidFormat => {
                "slug may only contain a-z, 0-9 and '-' and must start with a letter or digit"
            }
        })
    }
}

impl std::error::Error for SlugError {}

/// A URL-safe slug matching the database CHECK `^[a-z0-9][a-z0-9-]{0,62}$`.
/// Slugs are not trimmed or lowercased: input must already be canonical.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Slug(String);

impl Slug {
    /// Validates `raw`.
    ///
    /// # Errors
    ///
    /// A [`SlugError`] describing the violated rule.
    pub fn parse(raw: &str) -> Result<Self, SlugError> {
        if raw.is_empty() {
            return Err(SlugError::Empty);
        }
        if raw.len() > SLUG_MAX_CHARS {
            return Err(SlugError::TooLong);
        }
        let mut chars = raw.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if first_ok && rest_ok {
            Ok(Self(raw.to_owned()))
        } else {
            Err(SlugError::InvalidFormat)
        }
    }

    /// The validated slug.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_is_trimmed_and_lowercased() {
        let email = NormalizedEmail::parse("  Alice.Smith@Example.COM \n").unwrap();
        assert_eq!(email.as_str(), "alice.smith@example.com");
        assert_eq!(email.local_part(), "alice.smith");
    }

    #[test]
    fn email_rejects_structural_problems() {
        assert_eq!(NormalizedEmail::parse(""), Err(EmailError::Empty));
        assert_eq!(NormalizedEmail::parse("   "), Err(EmailError::Empty));
        assert_eq!(NormalizedEmail::parse("a@@b"), Err(EmailError::AtSignCount));
        assert_eq!(
            NormalizedEmail::parse("a@b@c"),
            Err(EmailError::AtSignCount)
        );
        assert_eq!(
            NormalizedEmail::parse("nobody"),
            Err(EmailError::AtSignCount)
        );
        assert_eq!(
            NormalizedEmail::parse("@example.com"),
            Err(EmailError::EmptyPart)
        );
        assert_eq!(NormalizedEmail::parse("a@"), Err(EmailError::EmptyPart));
    }

    #[test]
    fn email_rejects_whitespace_and_control_characters() {
        for bad in [
            "a b@c.d",
            "a@c .d",
            "a\t@c.d",
            "a\u{0}@c.d",
            "a@c.d\u{7f}",
            "a\u{a0}b@c.d",
            "a@c.d\u{2028}x",
        ] {
            assert_eq!(
                NormalizedEmail::parse(bad),
                Err(EmailError::ForbiddenCharacter),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn email_length_bounds_are_checked_after_normalization() {
        let local = "a".repeat(EMAIL_MAX_CHARS - 4);
        let ok = format!("{local}@b.c");
        assert_eq!(ok.chars().count(), EMAIL_MAX_CHARS);
        assert!(NormalizedEmail::parse(&ok).is_ok());
        let too_long = format!("a{ok}");
        assert_eq!(NormalizedEmail::parse(&too_long), Err(EmailError::TooLong));
        // Absurdly long input is rejected without full processing.
        let huge = "a".repeat(100_000);
        assert_eq!(NormalizedEmail::parse(&huge), Err(EmailError::TooLong));
    }

    #[test]
    fn email_unicode_handling() {
        let email = NormalizedEmail::parse("JÖRG@Bücher.Example").unwrap();
        assert_eq!(email.as_str(), "jörg@bücher.example");
        // Lowercasing may expand a character (U+0130 -> "i" + U+0307).
        let expanded = NormalizedEmail::parse("\u{130}@x.y").unwrap();
        assert_eq!(expanded.as_str(), "i\u{307}@x.y");
        // Expansion that pushes the result over the limit is rejected.
        let local = "\u{130}".repeat(EMAIL_MAX_CHARS - 3);
        assert_eq!(
            NormalizedEmail::parse(&format!("{local}@y")),
            Err(EmailError::TooLong)
        );
    }

    #[test]
    fn email_error_messages_do_not_echo_input() {
        let err = NormalizedEmail::parse("secret value@x").unwrap_err();
        assert!(!err.to_string().contains("secret"));
    }

    #[test]
    fn display_name_rules() {
        assert_eq!(
            DisplayName::parse("  Ada Lovelace ").unwrap().as_str(),
            "Ada Lovelace"
        );
        assert_eq!(DisplayName::parse(""), Err(DisplayNameError::Empty));
        assert_eq!(DisplayName::parse(" \t "), Err(DisplayNameError::Empty));
        assert_eq!(
            DisplayName::parse("a\nb"),
            Err(DisplayNameError::ControlCharacter)
        );
        assert_eq!(
            DisplayName::parse("a\u{0}"),
            Err(DisplayNameError::ControlCharacter)
        );
        // Invisible format characters are rejected, alone or inside text.
        for bad in [
            "\u{200B}",
            "Admin\u{200B}istrator",
            "\u{202E}gnp.exe",
            "evil\u{202E}fdp.exe",
            "a\u{2066}b\u{2069}",
            "a\u{200E}",
            "a\u{200F}",
            "\u{FEFF}name",
            "a\u{2060}b",
            "a\u{00AD}b",
            "a\u{E0041}b",
        ] {
            assert_eq!(
                DisplayName::parse(bad),
                Err(DisplayNameError::FormatCharacter),
                "{bad:?}"
            );
        }
        // Needs at least one visible character.
        for bad in ["\u{3164}", " \u{2800} ", "\u{115F}\u{1160}"] {
            assert_eq!(
                DisplayName::parse(bad),
                Err(DisplayNameError::NoVisibleCharacter),
                "{bad:?}"
            );
        }
        assert!(DisplayName::parse("A\u{3164}").is_ok());
        assert!(DisplayName::parse(&"x".repeat(DISPLAY_NAME_MAX_CHARS)).is_ok());
        assert_eq!(
            DisplayName::parse(&"x".repeat(DISPLAY_NAME_MAX_CHARS + 1)),
            Err(DisplayNameError::TooLong)
        );
        // Characters are counted, not bytes.
        assert!(DisplayName::parse(&"ü".repeat(DISPLAY_NAME_MAX_CHARS)).is_ok());
        assert!(DisplayName::parse(&"😀".repeat(DISPLAY_NAME_MAX_CHARS)).is_ok());
    }

    #[test]
    fn slug_rules() {
        for ok in [
            "a",
            "0",
            "acme",
            "acme-corp",
            "a-",
            "9-9",
            &"a".repeat(SLUG_MAX_CHARS),
        ] {
            assert!(Slug::parse(ok).is_ok(), "{ok}");
        }
        assert_eq!(Slug::parse(""), Err(SlugError::Empty));
        assert_eq!(
            Slug::parse(&"a".repeat(SLUG_MAX_CHARS + 1)),
            Err(SlugError::TooLong)
        );
        for bad in ["-a", "Acme", "a b", "a_b", "ä", "a.b", "a/b", "a\n"] {
            assert_eq!(Slug::parse(bad), Err(SlugError::InvalidFormat), "{bad:?}");
        }
        // Multi-byte input longer than the limit in bytes is rejected as too long.
        assert_eq!(Slug::parse(&"ä".repeat(40)), Err(SlugError::TooLong));
    }
}
