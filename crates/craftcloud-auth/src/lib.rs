//! Authentication primitives: password policy and hashing (Argon2id) and
//! constant-time comparison of bootstrap secrets.
//!
//! This crate has no HTTP, database or environment access. Hashing functions
//! are CPU- and memory-heavy and **blocking**; async callers must run them
//! on a blocking thread pool.

mod password;
mod token;

pub use password::{
    HashError, PASSWORD_MAX_CHARS, PASSWORD_MIN_CHARS, PasswordError, hash_password,
    validate_password, verify_password,
};
pub use token::{BOOTSTRAP_TOKEN_MIN_CHARS, BootstrapToken, TokenError};
