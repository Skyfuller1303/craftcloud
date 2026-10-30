//! Authentication primitives: password policy and hashing (Argon2id) and
//! constant-time comparison of bootstrap secrets.
//!
//! This crate has no HTTP, database or environment access. Hashing functions
//! are CPU- and memory-heavy and **blocking**; async callers must run them
//! on a blocking thread pool.

mod device;
mod opaque;
mod password;
mod secretbox;
mod token;
mod totp;

pub use device::{DeviceToken, DeviceTokenKind};
pub use opaque::{OPAQUE_TOKEN_CHARS, OpaqueToken, RandomnessError};
pub use password::{
    HashError, PASSWORD_MAX_CHARS, PASSWORD_MIN_CHARS, PasswordError, dummy_verify, hash_password,
    needs_rehash, validate_password, verify_or_dummy, verify_password,
};
pub use secretbox::{
    SecretBoxError, decrypt_secret, decrypt_secret_for, encrypt_secret, encrypt_secret_for,
};
pub use token::{BOOTSTRAP_TOKEN_MIN_CHARS, BootstrapToken, TokenError};
pub use totp::{
    TotpError, base32_decode, base32_encode, generate_totp_secret, recovery_codes, verify_totp,
};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Produces a unique salt and SHA-256 hash for a recovery code.
pub fn hash_recovery_code(code: &str) -> Result<([u8; 16], [u8; 32]), RandomnessError> {
    let mut salt = [0; 16];
    OsRng
        .try_fill_bytes(&mut salt)
        .map_err(|_| RandomnessError)?;
    Ok((salt, recovery_code_hash(&salt, code)))
}

/// Hashes a recovery code with its persisted salt and a domain separator.
#[must_use]
pub fn recovery_code_hash(salt: &[u8; 16], code: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"craftcloud-two-factor-recovery-v1:");
    digest.update(salt);
    digest.update(code.trim().to_ascii_uppercase().as_bytes());
    digest.finalize().into()
}

/// Constant-time comparison of a fixed-size recovery-code digest.
#[must_use]
pub fn recovery_hash_matches(expected: &[u8; 32], actual: &[u8; 32]) -> bool {
    bool::from(expected.ct_eq(actual))
}

/// Lifetime of a password reset link (ADR-0013).
pub const PASSWORD_RESET_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
