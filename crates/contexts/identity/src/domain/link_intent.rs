//! Linking intents: the account-linking challenge's single-use state.
//!
//! When an OIDC sign-in would collide with an existing account (unverified
//! email on either side), we refuse to link and instead store the pending
//! (provider, subject, email) behind a nonce the browser carries. Only a
//! user who then proves ownership of the existing account (regular login)
//! may complete the link.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::values::Email;

/// The browser-side nonce: random, single use, short-lived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkNonce(String);

impl LinkNonce {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        let mut rng = uuid::Uuid::now_v7().as_bytes().to_vec();
        while rng.len() < bytes.len() {
            rng.extend_from_slice(uuid::Uuid::now_v7().as_bytes());
        }
        let len = bytes.len();
        bytes.copy_from_slice(&rng[..len]);
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn parse(raw: &str) -> Result<Self, crate::domain::IdentityError> {
        let nonce = raw.trim();
        if nonce.len() != 64 || !nonce.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(crate::domain::IdentityError::MalformedWebhook(
                "invalid nonce shape".to_string(),
            ));
        }
        Ok(Self(nonce.to_ascii_lowercase()))
    }

    pub fn nonce_hash(&self) -> String {
        let digest = Sha256::digest(self.0.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What the intent promises to link, recorded verbatim for the completion
/// check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkIntentPayload {
    pub provider: String,
    pub subject: String,
    pub email: String,
    /// The Kratos login flow this intent is bound to (correlates audit).
    pub kratos_flow_id: Option<String>,
}

impl LinkIntentPayload {
    pub fn email(&self) -> Option<Email> {
        Email::parse(&self.email).ok()
    }
}

/// A stored, not-yet-consumed intent. Consumption is `take`: the adapter
/// deletes the row atomically, so replay can never complete twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkIntent {
    nonce_hash: String,
    payload: LinkIntentPayload,
    expires_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
}

impl LinkIntent {
    pub fn new(
        nonce: &LinkNonce,
        payload: LinkIntentPayload,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            nonce_hash: nonce.nonce_hash(),
            payload,
            expires_at,
            created_at: now,
        }
    }

    pub fn from_parts(
        nonce_hash: String,
        payload: LinkIntentPayload,
        expires_at: DateTime<Utc>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            nonce_hash,
            payload,
            expires_at,
            created_at,
        }
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn nonce_hash(&self) -> &str {
        &self.nonce_hash
    }
    pub fn payload(&self) -> &LinkIntentPayload {
        &self.payload
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}
