//! Opaque random bearer tokens (session cookies, password reset links).
//!
//! A token is 32 bytes from the operating system random source, encoded as
//! 43 characters of unpadded base64url. The server stores only the SHA-256
//! hash. The CSRF token of a session is derived from the raw session token,
//! so it needs no storage, can be returned to the cookie holder again at any
//! time, and cannot be computed from the stored hash.

use std::fmt;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// Length of an encoded token in characters.
pub const OPAQUE_TOKEN_CHARS: usize = 43;

const CSRF_DOMAIN: &[u8] = b"craftcloud-csrf-v1:";

/// The operating system random source failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RandomnessError;

impl fmt::Display for RandomnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the operating system random source failed")
    }
}

impl std::error::Error for RandomnessError {}

/// A raw bearer token. Never printed; zeroed on drop.
pub struct OpaqueToken(String);

impl OpaqueToken {
    /// Generates a fresh token.
    ///
    /// # Errors
    ///
    /// [`RandomnessError`] if the random source fails (no panic).
    pub fn generate() -> Result<Self, RandomnessError> {
        let mut bytes = [0u8; 32];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| RandomnessError)?;
        let encoded = URL_SAFE_NO_PAD.encode(bytes);
        bytes.zeroize();
        Ok(Self(encoded))
    }

    /// Parses a token presented by a client (cookie value, reset link).
    /// Returns `None` unless it has exactly the generated shape, which also
    /// bounds the work done for garbage input.
    #[must_use]
    pub fn parse(candidate: &str) -> Option<Self> {
        let valid = candidate.len() == OPAQUE_TOKEN_CHARS
            && candidate
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        valid.then(|| Self(candidate.to_owned()))
    }

    /// The token text (for the cookie or reset link). Callers must not log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// SHA-256 of the token: the only form that is stored.
    #[must_use]
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }

    /// The CSRF token bound to this (session) token.
    #[must_use]
    pub fn csrf_token(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(CSRF_DOMAIN);
        hasher.update(self.0.as_bytes());
        URL_SAFE_NO_PAD.encode(hasher.finalize())
    }

    /// Constant-time check of a client-supplied CSRF token.
    #[must_use]
    pub fn csrf_matches(&self, candidate: &str) -> bool {
        let expected: [u8; 32] = Sha256::digest(self.csrf_token().as_bytes()).into();
        let given: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        expected.ct_eq(&given).into()
    }
}

impl Drop for OpaqueToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for OpaqueToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OpaqueToken([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_have_the_expected_shape_and_differ() {
        let a = OpaqueToken::generate().unwrap();
        let b = OpaqueToken::generate().unwrap();
        assert_eq!(a.expose().len(), OPAQUE_TOKEN_CHARS);
        assert_ne!(a.expose(), b.expose());
        assert!(OpaqueToken::parse(a.expose()).is_some());
    }

    #[test]
    fn parse_rejects_wrong_shapes() {
        for bad in [
            "",
            "short",
            &"a".repeat(42),
            &"a".repeat(44),
            &format!("{}!", "a".repeat(42)),
            &format!("{} ", "a".repeat(42)),
            &"ü".repeat(22),
        ] {
            assert!(OpaqueToken::parse(bad).is_none(), "{bad:?}");
        }
        assert!(OpaqueToken::parse(&"a".repeat(43)).is_some());
        assert!(OpaqueToken::parse(&format!("{}-_", "A".repeat(41))).is_some());
    }

    #[test]
    fn hash_is_stable_and_not_the_token() {
        let t = OpaqueToken::parse(&"a".repeat(43)).unwrap();
        assert_eq!(t.hash(), t.hash());
        assert_ne!(t.hash().as_slice(), t.expose().as_bytes());
    }

    #[test]
    fn csrf_token_is_bound_to_the_session_token() {
        let a = OpaqueToken::generate().unwrap();
        let b = OpaqueToken::generate().unwrap();
        let csrf = a.csrf_token();
        assert!(a.csrf_matches(&csrf));
        assert!(!b.csrf_matches(&csrf));
        assert!(!a.csrf_matches(""));
        assert!(!a.csrf_matches(&format!("{csrf}x")));
        assert!(!a.csrf_matches(a.expose()));
        assert_ne!(csrf, a.expose());
        assert_eq!(csrf.len(), 43);
    }

    #[test]
    fn debug_does_not_leak() {
        let t = OpaqueToken::generate().unwrap();
        assert!(!format!("{t:?}").contains(t.expose()));
    }
}
