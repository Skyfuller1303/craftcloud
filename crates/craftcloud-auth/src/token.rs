//! Bootstrap token handling with constant-time comparison.

use std::fmt;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Minimum length of a bootstrap token in characters after trimming.
pub const BOOTSTRAP_TOKEN_MIN_CHARS: usize = 32;

/// Why a configured bootstrap token was rejected. Never contains the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// Shorter than [`BOOTSTRAP_TOKEN_MIN_CHARS`] after trimming.
    TooShort,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bootstrap token must be at least {BOOTSTRAP_TOKEN_MIN_CHARS} characters long after trimming"
        )
    }
}

impl std::error::Error for TokenError {}

/// The configured bootstrap token, kept only as a SHA-256 digest.
///
/// Candidates are compared by hashing them and comparing the fixed-length
/// digests in constant time, so neither the length nor the content of the
/// token leaks through timing.
#[derive(Clone)]
pub struct BootstrapToken {
    digest: [u8; 32],
}

impl BootstrapToken {
    /// Creates the verifier from the token text (surrounding whitespace is
    /// trimmed).
    ///
    /// # Errors
    ///
    /// [`TokenError::TooShort`] if the trimmed token has fewer than
    /// [`BOOTSTRAP_TOKEN_MIN_CHARS`] characters.
    pub fn new(token: &str) -> Result<Self, TokenError> {
        let token = token.trim();
        if token.chars().take(BOOTSTRAP_TOKEN_MIN_CHARS).count() < BOOTSTRAP_TOKEN_MIN_CHARS {
            return Err(TokenError::TooShort);
        }
        Ok(Self {
            digest: Sha256::digest(token.as_bytes()).into(),
        })
    }

    /// Constant-time check of a candidate token (trimmed like the original).
    #[must_use]
    pub fn verify(&self, candidate: &str) -> bool {
        let candidate_digest: [u8; 32] = Sha256::digest(candidate.trim().as_bytes()).into();
        self.digest.ct_eq(&candidate_digest).into()
    }
}

impl fmt::Debug for BootstrapToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BootstrapToken([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn length_is_enforced_after_trim_in_characters() {
        assert!(BootstrapToken::new(TOKEN).is_ok());
        assert!(BootstrapToken::new(&format!("  {TOKEN}\n")).is_ok());
        assert_eq!(
            BootstrapToken::new(&TOKEN[..31]).unwrap_err(),
            TokenError::TooShort
        );
        assert_eq!(
            BootstrapToken::new(&format!("{}   ", &TOKEN[..31])).unwrap_err(),
            TokenError::TooShort
        );
        assert_eq!(BootstrapToken::new("").unwrap_err(), TokenError::TooShort);
        assert!(BootstrapToken::new(&"ü".repeat(32)).is_ok());
    }

    #[test]
    fn verifies_exact_token_only() {
        let token = BootstrapToken::new(TOKEN).unwrap();
        assert!(token.verify(TOKEN));
        assert!(token.verify(&format!(" {TOKEN}\n")));
        assert!(!token.verify(""));
        assert!(!token.verify(&TOKEN[..31]));
        assert!(!token.verify(&format!("{TOKEN}x")));
        assert!(!token.verify(&TOKEN.to_uppercase()));
    }

    #[test]
    fn debug_and_errors_do_not_leak() {
        let token = BootstrapToken::new(TOKEN).unwrap();
        assert!(!format!("{token:?}").contains("0123456789"));
        assert!(!TokenError::TooShort.to_string().contains("0123"));
    }
}
