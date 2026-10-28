//! Password policy and Argon2id hashing.

use std::fmt;
use std::sync::OnceLock;

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{
        PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
        rand_core::{OsRng, RngCore},
    },
};

/// Minimum password length in characters.
pub const PASSWORD_MIN_CHARS: usize = 12;
/// Maximum password length in characters (bounds hashing cost).
pub const PASSWORD_MAX_CHARS: usize = 128;

/// Argon2id parameters: OWASP minimum (19 MiB, 2 iterations, 1 lane).
const MEMORY_KIB: u32 = 19_456;
const ITERATIONS: u32 = 2;
const PARALLELISM: u32 = 1;

/// Why a password was rejected. Never contains the password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordError {
    /// Fewer than [`PASSWORD_MIN_CHARS`] characters.
    TooShort,
    /// More than [`PASSWORD_MAX_CHARS`] characters.
    TooLong,
    /// Equal to the local part of the account e-mail address.
    EqualsEmailLocalPart,
}

impl fmt::Display for PasswordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooShort => "password is too short",
            Self::TooLong => "password is too long",
            Self::EqualsEmailLocalPart => "password must not equal the e-mail local part",
        })
    }
}

impl std::error::Error for PasswordError {}

/// Checks the password policy: 12 to 128 characters (counted as characters,
/// not bytes), no composition rules, and not equal (case-insensitively) to
/// the local part of the account e-mail address.
///
/// # Errors
///
/// A [`PasswordError`] for the first violated rule.
pub fn validate_password(password: &str, email_local_part: &str) -> Result<(), PasswordError> {
    let count = password.chars().take(PASSWORD_MAX_CHARS + 1).count();
    if count < PASSWORD_MIN_CHARS {
        return Err(PasswordError::TooShort);
    }
    if count > PASSWORD_MAX_CHARS {
        return Err(PasswordError::TooLong);
    }
    // Compare lowercase character streams without allocating a lowercase
    // copy of the password (which would not be zeroized).
    if !email_local_part.is_empty()
        && password
            .chars()
            .flat_map(char::to_lowercase)
            .eq(email_local_part.chars().flat_map(char::to_lowercase))
    {
        return Err(PasswordError::EqualsEmailLocalPart);
    }
    Ok(())
}

fn hasher() -> Option<Argon2<'static>> {
    let params = Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, None).ok()?;
    Some(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Password hashing failed (random source or hasher error). Carries no
/// details and never the password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashError;

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("password hashing failed")
    }
}

impl std::error::Error for HashError {}

/// Hashes `password` with Argon2id and a fresh random 16-byte salt, returning
/// a PHC string. **Blocking** (about 20 MiB and tens of milliseconds); call
/// from a blocking thread in async code.
///
/// # Errors
///
/// [`HashError`] if the operating system random source or the hasher fails;
/// this function never panics on RNG failure.
pub fn hash_password(password: &str) -> Result<String, HashError> {
    let mut salt_bytes = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut salt_bytes)
        .map_err(|_| HashError)?;
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|_| HashError)?;
    hasher()
        .ok_or(HashError)?
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| HashError)
}

/// Whether a stored hash should be replaced after a successful verification:
/// anything that is not Argon2id v19 with exactly the current parameters
/// (including unparsable hashes).
#[must_use]
pub fn needs_rehash(phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return true;
    };
    if parsed.algorithm != Algorithm::Argon2id.ident() || parsed.version != Some(0x13) {
        return true;
    }
    match Params::try_from(&parsed) {
        Ok(params) => {
            params.m_cost() != MEMORY_KIB
                || params.t_cost() != ITERATIONS
                || params.p_cost() != PARALLELISM
        }
        Err(_) => true,
    }
}

static DUMMY_HASH: OnceLock<Option<String>> = OnceLock::new();

/// Spends the same work as a real verification and always returns `false`.
/// Used for unknown accounts so response time does not reveal whether an
/// account exists. **Blocking.** The first call also computes the dummy hash.
#[must_use]
pub fn dummy_verify(candidate: &str) -> bool {
    let dummy = DUMMY_HASH.get_or_init(|| hash_password("dummy password for equal timing").ok());
    if let Some(phc) = dummy {
        let _ = verify_password(phc, candidate);
    }
    false
}

/// Verifies against `stored` if there is one, otherwise performs a dummy
/// verification of equal cost. Returns `false` for a missing hash.
/// **Blocking.**
#[must_use]
pub fn verify_or_dummy(stored: Option<&str>, candidate: &str) -> bool {
    match stored {
        Some(phc) => verify_password(phc, candidate),
        None => dummy_verify(candidate),
    }
}

/// Verifies `password` against a PHC string. Any error (malformed hash,
/// unsupported algorithm) counts as a mismatch. **Blocking**.
#[must_use]
pub fn verify_password(phc: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_bounds_count_characters() {
        assert_eq!(
            validate_password(&"a".repeat(11), "x"),
            Err(PasswordError::TooShort)
        );
        assert_eq!(validate_password(&"a".repeat(12), "x"), Ok(()));
        assert_eq!(validate_password(&"a".repeat(128), "x"), Ok(()));
        assert_eq!(
            validate_password(&"a".repeat(129), "x"),
            Err(PasswordError::TooLong)
        );
        // 12 multi-byte characters are long enough although they use 48 bytes.
        assert_eq!(validate_password(&"😀".repeat(12), "x"), Ok(()));
        assert_eq!(
            validate_password(&"😀".repeat(11), "x"),
            Err(PasswordError::TooShort)
        );
        assert_eq!(
            validate_password(&"ü".repeat(129), "x"),
            Err(PasswordError::TooLong)
        );
        assert_eq!(validate_password("", "x"), Err(PasswordError::TooShort));
    }

    #[test]
    fn no_composition_rules() {
        assert_eq!(validate_password("aaaaaaaaaaaa", "x"), Ok(()));
        assert_eq!(validate_password("            ", "x"), Ok(()));
    }

    #[test]
    fn email_local_part_comparison_is_case_insensitive_and_unicode_aware() {
        assert_eq!(
            validate_password("JÖRG.SCHMIDT.X", "jörg.schmidt.x"),
            Err(PasswordError::EqualsEmailLocalPart)
        );
        assert_eq!(validate_password("another-long-password", ""), Ok(()));
    }

    #[test]
    fn rejects_email_local_part() {
        assert_eq!(
            validate_password("long.local.part", "Long.Local.Part"),
            Err(PasswordError::EqualsEmailLocalPart)
        );
        assert_eq!(
            validate_password("long.local.part!", "long.local.part"),
            Ok(())
        );
    }

    #[test]
    fn hash_is_argon2id_phc_with_expected_params_and_verifies() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(
            hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{hash}"
        );
        assert!(verify_password(&hash, "correct horse battery"));
        assert!(!verify_password(&hash, "correct horse batterY"));
        assert!(!verify_password(&hash, ""));
    }

    #[test]
    fn needs_rehash_detects_other_parameters_and_garbage() {
        let current = hash_password("some long password 1").unwrap();
        assert!(!needs_rehash(&current));
        let weaker = current.replace("m=19456", "m=8192");
        assert!(needs_rehash(&weaker));
        assert!(needs_rehash(&current.replace("t=2", "t=3")));
        assert!(needs_rehash(&current.replace("$argon2id$", "$argon2i$")));
        assert!(needs_rehash("garbage"));
        assert!(needs_rehash(""));
    }

    #[test]
    fn dummy_verification_never_succeeds() {
        assert!(!dummy_verify("dummy password for equal timing"));
        assert!(!verify_or_dummy(None, "anything at all here"));
        let hash = hash_password("right password 123").unwrap();
        assert!(verify_or_dummy(Some(&hash), "right password 123"));
        assert!(!verify_or_dummy(Some(&hash), "wrong password 123"));
    }

    #[test]
    fn salts_differ_between_hashes() {
        let a = hash_password("same password 123").unwrap();
        let b = hash_password("same password 123").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn malformed_hashes_never_verify() {
        for bad in [
            "",
            "plain",
            "$argon2id$",
            "$2b$12$abcdefghijklmnopqrstuv",
            "$argon2id$v=19$m=1,t=1,p=1$AAAA$AAAA",
        ] {
            assert!(!verify_password(bad, "whatever password"), "{bad}");
        }
    }
}
