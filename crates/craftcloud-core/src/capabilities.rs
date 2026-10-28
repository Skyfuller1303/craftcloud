//! Typed server capabilities as returned by `GET /api/v1/info`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// API version advertised by this server build.
pub const API_VERSION: &str = "1.0";

/// Maximum length (in bytes, equal to characters because only ASCII is
/// allowed) of a [`FeatureId`].
pub const FEATURE_ID_MAX_LEN: usize = 64;

/// Reason why a string is not a valid [`FeatureId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureIdError {
    /// The identifier is empty.
    Empty,
    /// The identifier is longer than [`FEATURE_ID_MAX_LEN`].
    TooLong {
        /// Length of the rejected input in bytes.
        len: usize,
    },
    /// The identifier contains a character outside `a-z`, `0-9`, `.`, `-`, `_`.
    InvalidCharacter {
        /// The first offending character.
        found: char,
    },
}

impl fmt::Display for FeatureIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "feature identifier must not be empty"),
            Self::TooLong { len } => write!(
                f,
                "feature identifier is {len} bytes long, the maximum is {FEATURE_ID_MAX_LEN}"
            ),
            Self::InvalidCharacter { found } => write!(
                f,
                "feature identifier contains invalid character {found:?}; \
                 allowed are a-z, 0-9, '.', '-' and '_'"
            ),
        }
    }
}

impl std::error::Error for FeatureIdError {}

/// A validated feature identifier such as `uploads.resumable`.
///
/// Rules: non-empty, at most [`FEATURE_ID_MAX_LEN`] bytes, only lowercase
/// ASCII letters, ASCII digits, `.`, `-` and `_`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FeatureId(String);

impl FeatureId {
    /// Validates `value` and wraps it.
    ///
    /// # Errors
    ///
    /// Returns a [`FeatureIdError`] if the value is empty, too long or
    /// contains a character outside the allowed set.
    pub fn new(value: impl Into<String>) -> Result<Self, FeatureIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(FeatureIdError::Empty);
        }
        if value.len() > FEATURE_ID_MAX_LEN {
            return Err(FeatureIdError::TooLong { len: value.len() });
        }
        if let Some(found) = value.chars().find(|c| !is_allowed_char(*c)) {
            return Err(FeatureIdError::InvalidCharacter { found });
        }
        Ok(Self(value))
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_allowed_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_')
}

impl TryFrom<String> for FeatureId {
    type Error = FeatureIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<FeatureId> for String {
    fn from(value: FeatureId) -> Self {
        value.0
    }
}

impl fmt::Display for FeatureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Operating mode of the server, so clients can tell a prototype apart from a
/// production deployment.
///
/// Unknown wire values are mapped to [`ServerMode::Unknown`] when
/// deserializing (controlled fallback, see
/// `docs/02-architecture/api-conventions.md`), so a newer server never breaks
/// an older client. A server must never advertise `Unknown` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServerMode {
    /// Local development prototype; not production-ready.
    #[serde(rename = "development-prototype")]
    DevelopmentPrototype,
    /// A mode this client version does not know.
    #[serde(rename = "unknown", other)]
    Unknown,
}

/// Server capabilities, the typed body of `GET /api/v1/info`.
///
/// `api_version` and `features` are the original contract fields;
/// `server_mode` is an additive field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// API version, currently [`API_VERSION`].
    pub api_version: String,
    /// Optional features this server supports.
    pub features: Vec<FeatureId>,
    /// Operating mode of this server.
    pub server_mode: ServerMode,
}

impl Capabilities {
    /// Default capabilities of the development prototype.
    ///
    /// The feature list is deliberately empty: PR-001 implements no optional
    /// feature, and `health` is a root-level operational endpoint rather than
    /// a negotiable API feature. `server_mode` makes the prototype nature
    /// explicit.
    #[must_use]
    pub fn development_defaults() -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            features: Vec::new(),
            server_mode: ServerMode::DevelopmentPrototype,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_identifiers() {
        for ok in ["health", "uploads.resumable", "a-b_c.9", "0"] {
            assert_eq!(FeatureId::new(ok).map(|f| f.to_string()), Ok(ok.to_owned()));
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(FeatureId::new(""), Err(FeatureIdError::Empty));
    }

    #[test]
    fn length_bounds() {
        let max = "a".repeat(FEATURE_ID_MAX_LEN);
        assert!(FeatureId::new(max).is_ok());
        let too_long = "a".repeat(FEATURE_ID_MAX_LEN + 1);
        assert_eq!(
            FeatureId::new(too_long),
            Err(FeatureIdError::TooLong {
                len: FEATURE_ID_MAX_LEN + 1
            })
        );
    }

    #[test]
    fn rejects_invalid_characters() {
        for (bad, found) in [
            ("Health", 'H'),
            ("a b", ' '),
            ("a/b", '/'),
            ("a\nb", '\n'),
            ("ü", 'ü'),
            ("a\0", '\0'),
        ] {
            assert_eq!(
                FeatureId::new(bad),
                Err(FeatureIdError::InvalidCharacter { found })
            );
        }
    }

    #[test]
    fn oversized_non_ascii_is_rejected_without_panic() {
        // Length is checked in bytes before any character handling.
        let input = "ü".repeat(FEATURE_ID_MAX_LEN);
        assert!(matches!(
            FeatureId::new(input),
            Err(FeatureIdError::TooLong { .. })
        ));
    }

    #[test]
    fn error_messages_are_descriptive() {
        assert!(FeatureIdError::Empty.to_string().contains("empty"));
        assert!(
            FeatureIdError::TooLong { len: 99 }
                .to_string()
                .contains("99")
        );
        assert!(
            FeatureIdError::InvalidCharacter { found: 'X' }
                .to_string()
                .contains("'X'")
        );
    }

    #[test]
    fn defaults_are_explicitly_development() {
        let caps = Capabilities::development_defaults();
        assert_eq!(caps.api_version, "1.0");
        assert!(caps.features.is_empty());
        assert_eq!(caps.server_mode, ServerMode::DevelopmentPrototype);
    }

    #[test]
    fn serializes_to_contract_shape() {
        let value = serde_json::to_value(Capabilities::development_defaults());
        assert_eq!(
            value.ok(),
            Some(serde_json::json!({
                "api_version": "1.0",
                "features": [],
                "server_mode": "development-prototype"
            }))
        );
    }

    #[test]
    fn serde_roundtrip() {
        let caps = Capabilities {
            api_version: API_VERSION.to_owned(),
            features: vec![
                FeatureId::new("a.b").unwrap(),
                FeatureId::new("c-d").unwrap(),
            ],
            server_mode: ServerMode::DevelopmentPrototype,
        };
        let json = serde_json::to_string(&caps).unwrap();
        let back: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(back, caps);
    }

    #[test]
    fn deserialization_validates_feature_ids() {
        let json =
            r#"{"api_version":"1.0","features":["Bad Id"],"server_mode":"development-prototype"}"#;
        assert!(serde_json::from_str::<Capabilities>(json).is_err());
        let json = r#"{"api_version":"1.0","features":[""],"server_mode":"development-prototype"}"#;
        assert!(serde_json::from_str::<Capabilities>(json).is_err());
    }

    #[test]
    fn unknown_server_mode_falls_back_to_unknown() {
        let json = r#"{"api_version":"1.0","features":[],"server_mode":"quantum-cloud"}"#;
        let caps: Capabilities = serde_json::from_str(json).unwrap();
        assert_eq!(caps.server_mode, ServerMode::Unknown);
    }

    #[test]
    fn missing_server_mode_is_rejected() {
        let json = r#"{"api_version":"1.0","features":[]}"#;
        assert!(serde_json::from_str::<Capabilities>(json).is_err());
    }
}
