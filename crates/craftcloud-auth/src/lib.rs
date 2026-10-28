//! Authentication primitives: password policy and hashing (Argon2id) and
//! constant-time comparison of bootstrap secrets.
//!
//! This crate has no HTTP, database or environment access. Hashing functions
//! are CPU- and memory-heavy and **blocking**; async callers must run them
//! on a blocking thread pool.

mod opaque;
mod password;
mod token;

pub use opaque::{OPAQUE_TOKEN_CHARS, OpaqueToken, RandomnessError};
pub use password::{
    HashError, PASSWORD_MAX_CHARS, PASSWORD_MIN_CHARS, PasswordError, dummy_verify, hash_password,
    needs_rehash, validate_password, verify_or_dummy, verify_password,
};
pub use token::{BOOTSTRAP_TOKEN_MIN_CHARS, BootstrapToken, TokenError};

/// Lifetime of a password reset link (ADR-0013).
pub const PASSWORD_RESET_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
