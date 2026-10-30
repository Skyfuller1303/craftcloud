//! Authenticated encryption for small server-side secrets.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use uuid::Uuid;
use zeroize::Zeroize;

const NONCE_LEN: usize = 12;
const VERSION: u8 = 1;

/// An encryption or authentication failure that reveals no input material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretBoxError {
    InvalidKey,
    InvalidCiphertext,
    EncryptionFailed,
}

/// Encrypts `plaintext` with AES-256-GCM and a fresh, random nonce.
///
/// The returned bytes are version, nonce, then authenticated ciphertext.
pub fn encrypt_secret(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
    encrypt_with_aad(key, plaintext, &[VERSION])
}

fn encrypt_with_aad(
    key: &[u8; 32],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, SecretBoxError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| SecretBoxError::InvalidKey)?;
    let mut nonce_bytes = [0; NONCE_LEN];
    OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|_| SecretBoxError::EncryptionFailed)?;
    let nonce = Nonce::from(nonce_bytes);
    let mut ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| SecretBoxError::EncryptionFailed)?;
    let mut output = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
    output.push(VERSION);
    output.extend_from_slice(&nonce_bytes);
    output.append(&mut ciphertext);
    nonce_bytes.zeroize();
    Ok(output)
}

/// Decrypts a value returned by [`encrypt_secret`].
pub fn decrypt_secret(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
    decrypt_with_aad(key, ciphertext, &[VERSION])
}

/// Encrypts a secret bound to its owner and purpose. Prevents ciphertext
/// copied from another account or secret field from being accepted.
pub fn encrypt_secret_for(
    key: &[u8; 32],
    owner: Uuid,
    purpose: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, SecretBoxError> {
    let mut aad = Vec::with_capacity(17 + purpose.len());
    aad.push(VERSION);
    aad.extend_from_slice(owner.as_bytes());
    aad.extend_from_slice(purpose);
    encrypt_with_aad(key, plaintext, &aad)
}

/// Decrypts an owner and purpose bound secret.
pub fn decrypt_secret_for(
    key: &[u8; 32],
    ciphertext: &[u8],
    owner: Uuid,
    purpose: &[u8],
) -> Result<Vec<u8>, SecretBoxError> {
    let mut aad = Vec::with_capacity(17 + purpose.len());
    aad.push(VERSION);
    aad.extend_from_slice(owner.as_bytes());
    aad.extend_from_slice(purpose);
    decrypt_with_aad(key, ciphertext, &aad)
}

fn decrypt_with_aad(
    key: &[u8; 32],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, SecretBoxError> {
    let version = ciphertext.first().copied();
    if version != Some(VERSION) || ciphertext.len() <= 1 + NONCE_LEN + 16 {
        return Err(SecretBoxError::InvalidCiphertext);
    }
    let nonce_bytes: [u8; NONCE_LEN] = ciphertext
        .get(1..1 + NONCE_LEN)
        .ok_or(SecretBoxError::InvalidCiphertext)?
        .try_into()
        .map_err(|_| SecretBoxError::InvalidCiphertext)?;
    let encrypted = ciphertext
        .get(1 + NONCE_LEN..)
        .ok_or(SecretBoxError::InvalidCiphertext)?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| SecretBoxError::InvalidKey)?;
    let nonce = Nonce::from(nonce_bytes);
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: encrypted,
                aad,
            },
        )
        .map_err(|_| SecretBoxError::InvalidCiphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_uses_fresh_nonces() {
        let key = [7; 32];
        let first = encrypt_secret(&key, b"private TOTP secret").unwrap_or_default();
        let second = encrypt_secret(&key, b"private TOTP secret").unwrap_or_default();
        assert_ne!(first, second);
        assert_eq!(
            decrypt_secret(&key, &first).as_deref(),
            Ok(b"private TOTP secret".as_slice())
        );
    }

    #[test]
    fn wrong_key_and_tampering_are_rejected() {
        let ciphertext = encrypt_secret(&[1; 32], b"secret").unwrap_or_default();
        assert_eq!(
            decrypt_secret(&[2; 32], &ciphertext),
            Err(SecretBoxError::InvalidCiphertext)
        );
        let mut tampered = ciphertext;
        if let Some(byte) = tampered.last_mut() {
            *byte ^= 1;
        }
        assert_eq!(
            decrypt_secret(&[1; 32], &tampered),
            Err(SecretBoxError::InvalidCiphertext)
        );
    }

    #[test]
    fn ciphertext_is_bound_to_owner_and_purpose_and_rotation_works() {
        let owner = Uuid::from_u128(1);
        let old_key = [4; 32];
        let current_key = [5; 32];
        let ciphertext =
            encrypt_secret_for(&old_key, owner, b"totp", b"secret").unwrap_or_default();
        assert_eq!(
            decrypt_secret_for(&current_key, &ciphertext, owner, b"totp"),
            Err(SecretBoxError::InvalidCiphertext)
        );
        assert_eq!(
            decrypt_secret_for(&old_key, &ciphertext, Uuid::from_u128(2), b"totp"),
            Err(SecretBoxError::InvalidCiphertext)
        );
        assert_eq!(
            decrypt_secret_for(&old_key, &ciphertext, owner, b"other"),
            Err(SecretBoxError::InvalidCiphertext)
        );
        let clear = decrypt_secret_for(&old_key, &ciphertext, owner, b"totp").unwrap_or_default();
        let rotated = encrypt_secret_for(&current_key, owner, b"totp", &clear).unwrap_or_default();
        assert_eq!(
            decrypt_secret_for(&current_key, &rotated, owner, b"totp").as_deref(),
            Ok(b"secret".as_slice())
        );
    }
}
