//! RFC 6238 TOTP and bounded Base32 helpers.

use std::collections::BTreeSet;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use zeroize::Zeroize;

type HmacSha1 = Hmac<Sha1>;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const RECOVERY: &[u8; 32] = b"ABCDEFGHIJKMNPQRSTUVWXYZ23456789";
const SECRET_BYTES: usize = 20;
const CODE_BYTES: usize = 10;
const TIME_STEP: i64 = 30;

/// TOTP input or operating-system randomness failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TotpError {
    InvalidSecret,
    InvalidCode,
    InvalidTimestamp,
    RandomnessUnavailable,
}

/// Generates a fresh RFC 4226 size (160-bit) TOTP secret.
pub fn generate_totp_secret() -> Result<[u8; SECRET_BYTES], TotpError> {
    let mut secret = [0; SECRET_BYTES];
    OsRng
        .try_fill_bytes(&mut secret)
        .map_err(|_| TotpError::RandomnessUnavailable)?;
    Ok(secret)
}

/// Encodes bytes as unpadded RFC 4648 Base32.
#[must_use]
pub fn base32_encode(bytes: &[u8]) -> String {
    let capacity = bytes.len().saturating_mul(8).div_ceil(5);
    let mut output = String::with_capacity(capacity);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((buffer >> bits) & 31) as usize;
            if let Some(symbol) = BASE32.get(index) {
                output.push(char::from(*symbol));
            }
        }
    }
    if bits > 0 {
        let index = ((buffer << (5 - bits)) & 31) as usize;
        if let Some(symbol) = BASE32.get(index) {
            output.push(char::from(*symbol));
        }
    }
    output
}

/// Decodes bounded unpadded RFC 4648 Base32; lowercase ASCII is accepted.
pub fn base32_decode(encoded: &str) -> Result<Vec<u8>, TotpError> {
    if encoded.is_empty() || encoded.len() > 128 {
        return Err(TotpError::InvalidSecret);
    }
    let mut output = Vec::with_capacity(encoded.len().saturating_mul(5) / 8);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in encoded.bytes() {
        let byte = byte.to_ascii_uppercase();
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => return Err(TotpError::InvalidSecret),
        };
        buffer = (buffer << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    if bits > 0 && (buffer & ((1u32 << bits) - 1)) != 0 {
        return Err(TotpError::InvalidSecret);
    }
    if output.is_empty() {
        return Err(TotpError::InvalidSecret);
    }
    Ok(output)
}

fn code_for_step(secret: &[u8], step: u64) -> Result<String, TotpError> {
    if secret.len() != SECRET_BYTES {
        return Err(TotpError::InvalidSecret);
    }
    let mut mac = HmacSha1::new_from_slice(secret).map_err(|_| TotpError::InvalidSecret)?;
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest.get(19).copied().ok_or(TotpError::InvalidSecret)? & 0x0f);
    let slice = digest
        .get(offset..offset + 4)
        .ok_or(TotpError::InvalidSecret)?;
    let raw =
        u32::from_be_bytes(slice.try_into().map_err(|_| TotpError::InvalidSecret)?) & 0x7fff_ffff;
    Ok(format!("{:06}", raw % 1_000_000))
}

/// Verifies a six-digit code within the configured ±1 30-second steps.
/// Returns the accepted time-step so callers can persist replay protection.
pub fn verify_totp(
    secret: &[u8],
    code: &str,
    now_unix_seconds: i64,
    last_accepted_step: Option<i64>,
) -> Result<Option<i64>, TotpError> {
    if secret.len() != SECRET_BYTES {
        return Err(TotpError::InvalidSecret);
    }
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TotpError::InvalidCode);
    }
    if now_unix_seconds < 0 {
        return Err(TotpError::InvalidTimestamp);
    }
    let current = now_unix_seconds / TIME_STEP;
    for delta in [-1i64, 0, 1] {
        let Some(step) = current.checked_add(delta) else {
            continue;
        };
        if step < 0 || last_accepted_step.is_some_and(|last| step <= last) {
            continue;
        }
        let candidate = code_for_step(secret, step as u64)?;
        if bool::from(constant_time_equal(code.as_bytes(), candidate.as_bytes())) {
            return Ok(Some(step));
        }
    }
    Ok(None)
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> subtle::Choice {
    use subtle::ConstantTimeEq;
    if left.len() != right.len() {
        return subtle::Choice::from(0);
    }
    left.ct_eq(right)
}

/// Generates ten distinct ten-symbol one-time recovery codes.
pub fn recovery_codes() -> Result<Vec<String>, TotpError> {
    let mut codes = BTreeSet::new();
    while codes.len() < 10 {
        let mut bytes = [0; CODE_BYTES];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| TotpError::RandomnessUnavailable)?;
        let mut code = String::with_capacity(CODE_BYTES);
        for byte in bytes.iter() {
            let index = usize::from(*byte & 31);
            if let Some(symbol) = RECOVERY.get(index) {
                code.push(char::from(*symbol));
            }
        }
        bytes.zeroize();
        codes.insert(code);
    }
    Ok(codes.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn totp_secret() -> &'static [u8; SECRET_BYTES] {
        static SECRET: OnceLock<[u8; SECRET_BYTES]> = OnceLock::new();
        SECRET.get_or_init(|| [b'1'; SECRET_BYTES])
    }

    #[test]
    fn base32_rfc_4648_round_trips() {
        for (input, expected) in [
            (b"f".as_slice(), "MY"),
            (b"fo", "MZXQ"),
            (b"foo", "MZXW6"),
            (b"foobar", "MZXW6YTBOI"),
        ] {
            let encoded = base32_encode(input);
            assert_eq!(encoded, expected);
            assert_eq!(base32_decode(&encoded).as_deref(), Ok(input));
        }
        assert_eq!(base32_decode("mzxw6"), Ok(b"foo".to_vec()));
        assert!(base32_decode("invalid!").is_err());
        assert!(base32_decode("MZ").is_err());
        assert!(base32_decode(&"A".repeat(129)).is_err());
    }

    #[test]
    fn rfc_6238_sha1_test_vectors() {
        let secret = b"12345678901234567890";
        for (timestamp, expected) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(
                code_for_step(secret, (timestamp / TIME_STEP) as u64),
                Ok(expected.to_owned())
            );
        }
    }

    #[test]
    fn accepts_adjacent_steps_but_never_replays() {
        let current = code_for_step(totp_secret(), 100).unwrap_or_default();
        assert_eq!(
            verify_totp(totp_secret(), &current, 100 * 30, None),
            Ok(Some(100))
        );
        assert_eq!(
            verify_totp(totp_secret(), &current, 100 * 30, Some(100)),
            Ok(None)
        );
        let previous = code_for_step(totp_secret(), 99).unwrap_or_default();
        assert_eq!(
            verify_totp(totp_secret(), &previous, 100 * 30, None),
            Ok(Some(99))
        );
        let next = code_for_step(totp_secret(), 101).unwrap_or_default();
        assert_eq!(
            verify_totp(totp_secret(), &next, 100 * 30, None),
            Ok(Some(101))
        );
        let outside = code_for_step(totp_secret(), 102).unwrap_or_default();
        assert_eq!(
            verify_totp(totp_secret(), &outside, 100 * 30, None),
            Ok(None)
        );
        assert_eq!(
            verify_totp(totp_secret(), "12345x", 100 * 30, None),
            Err(TotpError::InvalidCode)
        );
        assert_eq!(
            verify_totp(totp_secret(), "123456", -1, None),
            Err(TotpError::InvalidTimestamp)
        );
        assert_eq!(
            verify_totp(totp_secret(), &current, 100 * 30 + 1, Some(100)),
            Ok(None)
        );
    }

    #[test]
    fn recovery_codes_are_unique_ten_symbols_without_ambiguous_characters() {
        let codes = recovery_codes().unwrap_or_default();
        assert_eq!(codes.len(), 10);
        let unique = codes.iter().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), 10);
        assert!(
            codes.iter().all(|code| {
                code.len() == 10 && code.bytes().all(|byte| RECOVERY.contains(&byte))
            })
        );
    }
}
