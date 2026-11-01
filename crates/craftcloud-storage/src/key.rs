//! Object keys: random UUID v4 in lowercase hex, the only input of storage
//! paths.

use std::fmt;

use uuid::Uuid;

/// Why a string is not a valid object key or temporary id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyError;

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a valid object key")
    }
}

impl std::error::Error for KeyError {}

/// A server generated object key: 32 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey(String);

pub(crate) fn is_hex32(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl ObjectKey {
    /// A fresh random key.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4().simple().to_string())
    }

    /// Parses a stored key (for example from the database). Accepts exactly
    /// 32 lowercase hex characters; nothing else can reach a path.
    ///
    /// # Errors
    ///
    /// [`KeyError`] for any other string.
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        if is_hex32(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(KeyError)
        }
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn shard1(&self) -> &str {
        self.0.get(0..2).unwrap_or("00")
    }

    pub(crate) fn shard2(&self) -> &str {
        self.0.get(2..4).unwrap_or("00")
    }

    pub(crate) fn is_generated_uuid(&self) -> bool {
        Uuid::parse_str(&self.0).is_ok_and(|uuid| uuid.get_version() == Some(uuid::Version::Random))
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_valid_and_distinct() {
        let a = ObjectKey::generate();
        assert_eq!(ObjectKey::parse(a.as_str()), Ok(a.clone()));
        assert_ne!(a, ObjectKey::generate());
        assert_eq!(a.as_str().len(), 32);
    }

    #[test]
    fn only_lowercase_hex_of_length_32_parses() {
        let ok = "0123456789abcdef0123456789abcdef";
        assert!(ObjectKey::parse(ok).is_ok());
        for bad in [
            "",
            "short",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "../../etc/passwd/0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcde/",
            "0123456789abcdef\\0123456789abcdef",
            "0123456789abcdef0123456789abcd\u{00e9}",
            "0123456789abcdef0123456789abcde\0",
        ] {
            assert!(ObjectKey::parse(bad).is_err(), "{bad:?}");
        }
    }
}
