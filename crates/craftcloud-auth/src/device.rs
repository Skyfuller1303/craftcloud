//! Opaque prefixed tokens for desktop device sessions.

use std::fmt;

use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::{OpaqueToken, RandomnessError};

/// Device token purpose, encoded in its public prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceTokenKind {
    /// Short-lived API access credential.
    Access,
    /// Rotating credential used to renew a device session.
    Refresh,
}

impl DeviceTokenKind {
    /// Stable log and scanner prefix.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Access => "ccd_at_",
            Self::Refresh => "ccd_rt_",
        }
    }
}

/// Raw device token. Debug output is redacted and drop zeroes its contents.
pub struct DeviceToken {
    kind: DeviceTokenKind,
    wire: String,
}

impl DeviceToken {
    /// Generates 256 random bits for the requested purpose.
    pub fn generate(kind: DeviceTokenKind) -> Result<Self, RandomnessError> {
        let raw = OpaqueToken::generate()?;
        Ok(Self {
            kind,
            wire: format!("{}{}", kind.prefix(), raw.expose()),
        })
    }

    /// Parses a bounded token with the exact purpose prefix.
    #[must_use]
    pub fn parse(candidate: &str, kind: DeviceTokenKind) -> Option<Self> {
        let raw = candidate.strip_prefix(kind.prefix())?;
        OpaqueToken::parse(raw)?;
        Some(Self {
            kind,
            wire: candidate.to_owned(),
        })
    }

    /// Full wire token. Callers must never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.wire
    }

    /// SHA-256 over the full prefixed wire token.
    #[must_use]
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.wire.as_bytes()).into()
    }

    /// Token purpose.
    #[must_use]
    pub const fn kind(&self) -> DeviceTokenKind {
        self.kind
    }
}

impl Drop for DeviceToken {
    fn drop(&mut self) {
        self.wire.zeroize();
    }
}

impl fmt::Debug for DeviceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceToken([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_prefixed_hashed_and_kind_bound() {
        let access = DeviceToken::generate(DeviceTokenKind::Access).unwrap();
        let refresh = DeviceToken::generate(DeviceTokenKind::Refresh).unwrap();
        let access_text = access.expose();
        let refresh_text = refresh.expose();
        assert!(access_text.starts_with("ccd_at_"));
        assert!(refresh_text.starts_with("ccd_rt_"));
        assert_eq!(
            DeviceToken::parse(access_text, DeviceTokenKind::Access)
                .unwrap()
                .hash(),
            access.hash()
        );
        assert!(DeviceToken::parse(access_text, DeviceTokenKind::Refresh).is_none());
        assert_ne!(access.hash().as_slice(), access_text.as_bytes());
        assert!(!format!("{access:?}").contains(access_text));
    }

    #[test]
    fn parser_rejects_wrong_prefix_and_shape() {
        assert!(DeviceToken::parse("ccd_at_short", DeviceTokenKind::Access).is_none());
        assert!(
            DeviceToken::parse(
                &format!("ccd_rt_{}", "a".repeat(43)),
                DeviceTokenKind::Access
            )
            .is_none()
        );
    }
}
