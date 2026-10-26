//! Envelope encryption: a per-subject DEK wrapped by a KEK held outside the
//! database. Crypto-shredding is the destruction of the DEK row — after it,
//! ciphertext is unrecoverable and the decrypt path returns the typed
//! `KeyShredded` error rather than a panic or a leak.
//!
//! Export artifacts additionally use a passphrase-derived key (PBKDF2),
//! delivered out of band.

// aes-gcm 0.10 (the current release) still sits on generic-array 0.14,
// which deprecated itself on modern toolchains; the deprecation is
// ecosystem noise, not an actionable migration for this code.
#![allow(deprecated)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use async_trait::async_trait;
use rand::RngCore;
use sha2::Sha256;

use sanvi_persistence::TxManager;

use crate::domain::{EnvelopeCrypto, PrivacyError, SubjectId, SubjectKeyRepository};

/// PBKDF2-HMAC-SHA256 with 600k iterations (OWASP recommendation), 32-byte
/// output, random 16-byte salt.
fn derive_passphrase_key(passphrase: &str, salt: &[u8]) -> Result<Vec<u8>, PrivacyError> {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, 600_000, &mut key);
    Ok(key.to_vec())
}

fn nonce_bytes() -> [u8; 12] {
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// The concrete `EnvelopeCrypto` implementation: AES-256-GCM, per-subject
/// DEKs stored wrapped in `privacy.subject_keys`, a KEK from configuration.
pub struct SqlxEnvelopeCrypto {
    tx_manager: TxManager,
    keys: Arc<dyn SubjectKeyRepository>,
    kek: [u8; 32],
    kek_id: String,
    erasure_ledger_dir: String,
    /// In-memory cache of unwrapped DEKs (bounded; keyed by subject id).
    dek_cache: Mutex<HashMap<i64, Arc<[u8; 32]>>>,
    /// Subjects whose key was destroyed in this process: refuse to serve
    /// stale cached DEKs after a shred.
    shredded: Mutex<HashMap<i64, ()>>,
}

impl SqlxEnvelopeCrypto {
    /// `kek` must be exactly 32 bytes (validated by `PrivacyConfig`).
    pub fn new(
        tx_manager: TxManager,
        keys: Arc<dyn SubjectKeyRepository>,
        kek: [u8; 32],
        kek_id: String,
    ) -> Self {
        Self {
            tx_manager,
            keys,
            kek,
            kek_id,
            erasure_ledger_dir: String::new(),
            dek_cache: Mutex::new(HashMap::new()),
            shredded: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_erasure_ledger(mut self, directory: String) -> Self {
        self.erasure_ledger_dir = directory;
        self
    }

    fn ledger_revoked(&self, subject_id: SubjectId) -> Result<bool, PrivacyError> {
        super::erasure_ledger::contains(&self.erasure_ledger_dir, subject_id)
    }

    /// The KEK id recorded with wrapped keys (rotate by adding a key; the id
    /// survives rotation).
    pub fn kek_id(&self) -> &str {
        &self.kek_id
    }

    async fn load_dek_from_store(&self, subject_id: SubjectId) -> Result<[u8; 32], PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_envelope_crypto", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let row = self
            .keys
            .find(&mut scope, subject_id)
            .await?
            .ok_or(PrivacyError::KeyShredded)?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        if row.destroyed_at.is_some() {
            return Err(PrivacyError::KeyShredded);
        }
        let dek: [u8; 32] = self
            .unwrap(&row.wrapped_dek, &row.kek_id)?
            .try_into()
            .map_err(|_| PrivacyError::EncryptionFailure("DEK must be 32 bytes".to_string()))?;
        if let Ok(mut cache) = self.dek_cache.lock() {
            if cache.len() > 10_000 {
                cache.clear();
            }
            cache.insert(subject_id.as_i64(), Arc::new(dek));
        }
        Ok(dek)
    }

    async fn fetch_dek(&self, subject_id: SubjectId) -> Result<[u8; 32], PrivacyError> {
        if self.ledger_revoked(subject_id)? {
            return Err(PrivacyError::KeyShredded);
        }
        if let Ok(shredded) = self.shredded.lock()
            && shredded.contains_key(&subject_id.as_i64())
        {
            return Err(PrivacyError::KeyShredded);
        }
        self.load_dek_from_store(subject_id).await
    }
}

#[async_trait]
impl EnvelopeCrypto for SqlxEnvelopeCrypto {
    async fn create_subject_key(&self, subject_id: SubjectId) -> Result<Vec<u8>, PrivacyError> {
        if self.ledger_revoked(subject_id)? {
            return Err(PrivacyError::KeyShredded);
        }
        if let Ok(shredded) = self.shredded.lock()
            && shredded.contains_key(&subject_id.as_i64())
        {
            return Err(PrivacyError::KeyShredded);
        }

        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_subject_key_create", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        if let Some(row) = self.keys.find(&mut scope, subject_id).await? {
            scope
                .commit()
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            if row.destroyed_at.is_some() {
                return Err(PrivacyError::KeyShredded);
            }
            let dek: [u8; 32] = self
                .unwrap(&row.wrapped_dek, &row.kek_id)?
                .try_into()
                .map_err(|_| PrivacyError::EncryptionFailure("DEK must be 32 bytes".to_string()))?;
            if let Ok(mut cache) = self.dek_cache.lock() {
                cache.insert(subject_id.as_i64(), Arc::new(dek));
            }
            return Ok(row.wrapped_dek);
        }

        let mut dek = [0u8; 32];
        OsRng.fill_bytes(&mut dek);
        let wrapped = self.wrap(&dek, &self.kek_id)?;
        self.keys
            .save(&mut scope, subject_id, wrapped, &self.kek_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        // Re-read after the INSERT .. ON CONFLICT so concurrent creators all
        // cache and use the one canonical DEK stored in the database.
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_subject_key_reload", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let row = self
            .keys
            .find(&mut scope, subject_id)
            .await?
            .ok_or(PrivacyError::KeyShredded)?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        if row.destroyed_at.is_some() {
            return Err(PrivacyError::KeyShredded);
        }
        let canonical: [u8; 32] = self
            .unwrap(&row.wrapped_dek, &row.kek_id)?
            .try_into()
            .map_err(|_| PrivacyError::EncryptionFailure("DEK must be 32 bytes".to_string()))?;
        if let Ok(mut cache) = self.dek_cache.lock() {
            cache.insert(subject_id.as_i64(), Arc::new(canonical));
        }
        Ok(row.wrapped_dek)
    }

    fn wrap(&self, dek: &[u8], _kek_id: &str) -> Result<Vec<u8>, PrivacyError> {
        let cipher = Aes256Gcm::new_from_slice(&self.kek)
            .map_err(|_| PrivacyError::EncryptionFailure("invalid KEK".to_string()))?;
        let nonce_bytes = nonce_bytes();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), dek)
            .map_err(|_| PrivacyError::EncryptionFailure("wrap failed".to_string()))?;
        let mut out = Vec::with_capacity(12 + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn unwrap(&self, wrapped: &[u8], _kek_id: &str) -> Result<Vec<u8>, PrivacyError> {
        if wrapped.len() < 13 {
            return Err(PrivacyError::EncryptionFailure(
                "wrapped key too short".to_string(),
            ));
        }
        let cipher = Aes256Gcm::new_from_slice(&self.kek)
            .map_err(|_| PrivacyError::EncryptionFailure("invalid KEK".to_string()))?;
        cipher
            .decrypt(Nonce::from_slice(&wrapped[..12]), &wrapped[12..])
            .map_err(|_| PrivacyError::EncryptionFailure("unwrap failed".to_string()))
    }

    async fn encrypt_for_subject(
        &self,
        subject_id: SubjectId,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, PrivacyError> {
        let dek = self.fetch_dek(subject_id).await?;
        let cipher = Aes256Gcm::new_from_slice(&dek)
            .map_err(|_| PrivacyError::EncryptionFailure("invalid DEK".to_string()))?;
        let nonce_bytes = nonce_bytes();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .map_err(|_| PrivacyError::EncryptionFailure("encrypt failed".to_string()))?;
        let mut out = Vec::with_capacity(12 + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    async fn decrypt_for_subject(
        &self,
        subject_id: SubjectId,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, PrivacyError> {
        if ciphertext.len() < 13 {
            return Err(PrivacyError::EncryptionFailure(
                "ciphertext too short".to_string(),
            ));
        }
        let dek = self.fetch_dek(subject_id).await?;
        let cipher = Aes256Gcm::new_from_slice(&dek)
            .map_err(|_| PrivacyError::EncryptionFailure("invalid DEK".to_string()))?;
        cipher
            .decrypt(Nonce::from_slice(&ciphertext[..12]), &ciphertext[12..])
            .map_err(|_| PrivacyError::EncryptionFailure("decrypt failed".to_string()))
    }

    async fn destroy_subject_key(&self, subject_id: SubjectId) -> Result<(), PrivacyError> {
        if !self.erasure_ledger_dir.is_empty() && !self.ledger_revoked(subject_id)? {
            use crate::domain::SubjectRepository;
            let mut scope = self
                .tx_manager
                .begin_unscoped("privacy_erasure_ledger", "privacy")
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            let subject = super::repos::SqlxSubjectRepository::new()
                .find_by_id(&mut scope, subject_id)
                .await?
                .ok_or_else(|| PrivacyError::SubjectNotFound(subject_id.to_string()))?;
            scope
                .commit()
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            let reference = serde_json::to_vec(&subject.subject_ref)
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            let wrapped = self.wrap(&reference, &self.kek_id)?;
            super::erasure_ledger::record(&self.erasure_ledger_dir, subject_id, wrapped)?;
        }

        if let Ok(mut cache) = self.dek_cache.lock() {
            cache.remove(&subject_id.as_i64());
        }
        if let Ok(mut shredded) = self.shredded.lock() {
            shredded.insert(subject_id.as_i64(), ());
        }
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_crypto_shred", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.keys.destroy(&mut scope, subject_id).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))
    }

    fn encrypt_with_passphrase(
        &self,
        plaintext: &[u8],
        passphrase: &str,
    ) -> Result<Vec<u8>, PrivacyError> {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        let key = derive_passphrase_key(passphrase, &salt)?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| PrivacyError::EncryptionFailure("invalid key".to_string()))?;
        let nonce_bytes = nonce_bytes();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .map_err(|_| PrivacyError::EncryptionFailure("encrypt failed".to_string()))?;
        let mut out = Vec::with_capacity(16 + 12 + ciphertext.len());
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn decrypt_with_passphrase(
        &self,
        ciphertext: &[u8],
        passphrase: &str,
    ) -> Result<Vec<u8>, PrivacyError> {
        if ciphertext.len() < 16 + 12 + 16 {
            return Err(PrivacyError::EncryptionFailure(
                "ciphertext too short".to_string(),
            ));
        }
        let (salt, rest) = ciphertext.split_at(16);
        let (nonce_bytes, body) = rest.split_at(12);
        let key = derive_passphrase_key(passphrase, salt)?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| PrivacyError::EncryptionFailure("invalid key".to_string()))?;
        cipher
            .decrypt(Nonce::from_slice(nonce_bytes), body)
            .map_err(|_| PrivacyError::EncryptionFailure("decrypt failed".to_string()))
    }
}

/// A disabled crypto adapter: every operation fails closed. Used when no KEK
/// is configured — envelope encryption, export packaging and crypto-shred
/// refuse to run rather than silently degrade.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledEnvelopeCrypto;

#[async_trait]
impl EnvelopeCrypto for DisabledEnvelopeCrypto {
    async fn create_subject_key(&self, _subject_id: SubjectId) -> Result<Vec<u8>, PrivacyError> {
        Err(PrivacyError::EncryptionUnconfigured)
    }

    fn wrap(&self, _dek: &[u8], _kek_id: &str) -> Result<Vec<u8>, PrivacyError> {
        Err(PrivacyError::EncryptionUnconfigured)
    }

    fn unwrap(&self, _wrapped: &[u8], _kek_id: &str) -> Result<Vec<u8>, PrivacyError> {
        Err(PrivacyError::EncryptionUnconfigured)
    }

    async fn encrypt_for_subject(
        &self,
        _subject_id: SubjectId,
        _plaintext: &[u8],
    ) -> Result<Vec<u8>, PrivacyError> {
        Err(PrivacyError::EncryptionUnconfigured)
    }

    async fn decrypt_for_subject(
        &self,
        _subject_id: SubjectId,
        _ciphertext: &[u8],
    ) -> Result<Vec<u8>, PrivacyError> {
        Err(PrivacyError::EncryptionUnconfigured)
    }

    async fn destroy_subject_key(&self, _subject_id: SubjectId) -> Result<(), PrivacyError> {
        Err(PrivacyError::EncryptionUnconfigured)
    }

    fn encrypt_with_passphrase(
        &self,
        _plaintext: &[u8],
        _passphrase: &str,
    ) -> Result<Vec<u8>, PrivacyError> {
        Err(PrivacyError::EncryptionUnconfigured)
    }

    fn decrypt_with_passphrase(
        &self,
        _ciphertext: &[u8],
        _passphrase: &str,
    ) -> Result<Vec<u8>, PrivacyError> {
        Err(PrivacyError::EncryptionUnconfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crypto() -> SqlxEnvelopeCrypto {
        // No DB needed for wrap/unwrap/passphrase tests.
        let tx = TxManager::new(
            sanvi_persistence::PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
                .expect("lazy pool"),
            sanvi_testsupport::test_ids(),
        );
        SqlxEnvelopeCrypto::new(
            tx,
            Arc::new(crate::adapters::repos::SqlxSubjectKeyRepository::new()),
            [9u8; 32],
            "test-kek".to_string(),
        )
    }

    #[tokio::test]
    async fn wrap_unwrap_roundtrips() {
        let crypto = crypto();
        let dek = [3u8; 32];
        let wrapped = crypto.wrap(&dek, "test-kek").unwrap();
        let unwrapped = crypto.unwrap(&wrapped, "test-kek").unwrap();
        assert_eq!(unwrapped, dek);
    }

    #[tokio::test]
    async fn passphrase_roundtrips() {
        let crypto = crypto();
        let plaintext = b"the subject's export data";
        let ciphertext = crypto
            .encrypt_with_passphrase(plaintext, "one-time-passphrase")
            .unwrap();
        let decrypted = crypto
            .decrypt_with_passphrase(&ciphertext, "one-time-passphrase")
            .unwrap();
        assert_eq!(decrypted, plaintext);
        assert!(
            crypto
                .decrypt_with_passphrase(&ciphertext, "wrong")
                .is_err()
        );
    }

    #[tokio::test]
    async fn disabled_crypto_fails_closed() {
        let crypto = DisabledEnvelopeCrypto;
        let result = crypto
            .encrypt_for_subject(SubjectId::try_from(1_i64).unwrap(), b"x")
            .await;
        assert_eq!(result.unwrap_err(), PrivacyError::EncryptionUnconfigured);
        assert_eq!(
            crypto.decrypt_with_passphrase(b"x", "p").unwrap_err(),
            PrivacyError::EncryptionUnconfigured
        );
    }
}
