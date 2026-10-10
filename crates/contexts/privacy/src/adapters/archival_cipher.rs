//! The archival cipher bridge (TASK-028, FR-1129): the archival crate's
//! seal port implemented over the phase-05 envelope scheme's passphrase
//! layer.
//!
//! Cold archive objects are sealed with the same
//! `EnvelopeCrypto::encrypt_with_passphrase` machinery the privacy
//! exports use (PBKDF2-HMAC-SHA256, 600k iterations + AES-256-GCM). The
//! passphrase is derived from the configured KEK with a fixed context
//! label — specific to archival, never equal to the KEK itself — so
//! archives stay decryptable across worker restarts without a
//! per-archive secret to babysit. KEK rotation rewraps subject DEKs but
//! does not rewrite archives; the rotation runbook covers the
//! archive-and-restore-under-both-keys drill when it matters.

use std::sync::Arc;

use sanvi_archival::ArchivalError;
use sanvi_archival::storage::ArchiveCipher;

use crate::domain::ports::EnvelopeCrypto;

/// The fixed context label binding the archival passphrase to its
/// purpose.
const PASSPHRASE_CONTEXT: &[u8] = b"sanvi-archival-v1";

/// Derive the archival passphrase from the configured KEK bytes.
pub fn archival_passphrase(kek: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(kek).expect("hmac accepts any key length");
    mac.update(PASSPHRASE_CONTEXT);
    let digest = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The bridge: archival's seal port over the envelope scheme.
pub struct EnvelopeArchiveCipher {
    crypto: Arc<dyn EnvelopeCrypto>,
    passphrase: String,
}

impl EnvelopeArchiveCipher {
    pub fn new(crypto: Arc<dyn EnvelopeCrypto>, kek: &[u8]) -> Self {
        Self {
            crypto,
            passphrase: archival_passphrase(kek),
        }
    }
}

impl ArchiveCipher for EnvelopeArchiveCipher {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, ArchivalError> {
        self.crypto
            .encrypt_with_passphrase(plaintext, &self.passphrase)
            .map_err(|err| ArchivalError::Cipher(err.to_string()))
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, ArchivalError> {
        self.crypto
            .decrypt_with_passphrase(sealed, &self.passphrase)
            .map_err(|err| ArchivalError::Cipher(err.to_string()))
    }
}
