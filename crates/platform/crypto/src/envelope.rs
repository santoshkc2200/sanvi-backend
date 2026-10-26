//! AES-256-GCM sealing in the `nonce (12 bytes) || ciphertext+tag` layout.
//!
//! The same function wraps a DEK under a KEK and seals data under a DEK; the
//! caller decides which key is which.

// aes-gcm 0.10 still sits on generic-array 0.14, which deprecated itself on
// modern toolchains; the deprecation is ecosystem noise, not a migration.
#![allow(deprecated)]

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use rand::RngCore;

pub type Key = [u8; 32];

const NONCE_LEN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    #[error("ciphertext is too short")]
    Truncated,
    #[error("encryption failed")]
    Seal,
    #[error("decryption failed")]
    Open,
}

/// A fresh random 256-bit key, for a new DEK.
pub fn generate_key() -> Key {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    key
}

pub fn seal(key: &Key, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::Seal)?;
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| CryptoError::Seal)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub fn open(key: &Key, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if sealed.len() <= NONCE_LEN {
        return Err(CryptoError::Truncated);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::Open)?;
    cipher
        .decrypt(
            Nonce::from_slice(&sealed[..NONCE_LEN]),
            &sealed[NONCE_LEN..],
        )
        .map_err(|_| CryptoError::Open)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrips_and_uses_a_fresh_nonce() {
        let key = generate_key();
        let a = seal(&key, b"refresh-token").unwrap();
        let b = seal(&key, b"refresh-token").unwrap();
        assert_ne!(a, b);
        assert_eq!(open(&key, &a).unwrap(), b"refresh-token");
    }

    #[test]
    fn open_rejects_wrong_key_tampering_and_truncation() {
        let key = generate_key();
        let mut sealed = seal(&key, b"secret").unwrap();
        assert_eq!(open(&generate_key(), &sealed), Err(CryptoError::Open));
        assert_eq!(open(&key, &sealed[..12]), Err(CryptoError::Truncated));
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert_eq!(open(&key, &sealed), Err(CryptoError::Open));
    }
}
