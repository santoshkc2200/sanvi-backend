//! The cold-storage port and its local encrypted volume implementation.
//!
//! The mover never touches a filesystem directly: everything goes through
//! [`ColdStorage`] with opaque, storage-relative paths. The parked
//! production target (object storage) is a second implementation of this
//! trait; nothing else changes when it lands.

use std::path::PathBuf;
use std::sync::Arc;

use crate::ArchivalError;

/// Symmetric seal/open over archive bytes. Composition roots implement
/// this over the phase-05 envelope scheme's passphrase form
/// (`EnvelopeCrypto::encrypt_with_passphrase`), so the seal is
/// PBKDF2-HMAC-SHA256 + AES-256-GCM with the KEK-derived passphrase —
/// the same layer the privacy exports are encrypted with.
pub trait ArchiveCipher: Send + Sync {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, ArchivalError>;
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, ArchivalError>;
}

/// Opaque cold storage for sealed archive objects.
pub trait ColdStorage: Send + Sync {
    fn put(&self, path: &str, sealed: &[u8]) -> Result<(), ArchivalError>;
    fn get(&self, path: &str) -> Result<Vec<u8>, ArchivalError>;
    fn delete(&self, path: &str) -> Result<(), ArchivalError>;
}

/// The default adapter: a local volume holding sealed objects (sealing
/// itself is [`SealedStorage`]'s job, composing this with an
/// [`ArchiveCipher`]). The interface is deliberately the object-store
/// shape (`put`/`get`/`delete` by key, no partial writes, no listing —
/// the registry is the database's), so swapping in an S3-backed adapter
/// later is a composition change, not a mover change.
#[derive(Clone)]
pub struct LocalEncryptedVolume {
    root: PathBuf,
}

impl LocalEncryptedVolume {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve a storage key to a filesystem path, refusing traversal:
    /// archive paths are registry-minted (`schema/table/scope/name.enc`),
    /// and the refusal keeps a tampered registry row from writing outside
    /// the volume.
    fn resolve(&self, path: &str) -> Result<PathBuf, ArchivalError> {
        let full = self.root.join(path);
        if !full.starts_with(&self.root) {
            return Err(ArchivalError::Cold(format!(
                "storage path {path:?} escapes the volume root"
            )));
        }
        Ok(full)
    }
}

impl ColdStorage for LocalEncryptedVolume {
    fn put(&self, path: &str, sealed: &[u8]) -> Result<(), ArchivalError> {
        let target = self.resolve(path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| ArchivalError::Cold(format!("create {parent:?}: {err}")))?;
        }
        // Write-then-rename: a reader never observes a half-written object.
        let staging = target.with_extension("part");
        std::fs::write(&staging, sealed)
            .map_err(|err| ArchivalError::Cold(format!("write {staging:?}: {err}")))?;
        std::fs::rename(&staging, &target)
            .map_err(|err| ArchivalError::Cold(format!("rename to {target:?}: {err}")))?;
        Ok(())
    }

    fn get(&self, path: &str) -> Result<Vec<u8>, ArchivalError> {
        let target = self.resolve(path)?;
        std::fs::read(&target).map_err(|err| ArchivalError::Cold(format!("read {target:?}: {err}")))
    }

    fn delete(&self, path: &str) -> Result<(), ArchivalError> {
        let target = self.resolve(path)?;
        match std::fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(ArchivalError::Cold(format!("delete {target:?}: {err}"))),
        }
    }
}

/// Sealed helpers: encrypt-on-put / decrypt-on-get so cold bytes are only
/// ever plaintext in memory.
pub struct SealedStorage {
    storage: Arc<dyn ColdStorage>,
    cipher: Arc<dyn ArchiveCipher>,
}

impl SealedStorage {
    pub fn new(storage: Arc<dyn ColdStorage>, cipher: Arc<dyn ArchiveCipher>) -> Self {
        Self { storage, cipher }
    }

    pub fn put(&self, path: &str, plaintext: &[u8]) -> Result<u64, ArchivalError> {
        let sealed = self.cipher.seal(plaintext)?;
        self.storage.put(path, &sealed)?;
        Ok(sealed.len() as u64)
    }

    pub fn get(&self, path: &str) -> Result<Vec<u8>, ArchivalError> {
        let sealed = self.storage.get(path)?;
        self.cipher.open(&sealed)
    }

    pub fn delete(&self, path: &str) -> Result<(), ArchivalError> {
        self.storage.delete(path)
    }
}
