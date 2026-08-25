//! Per-tenant envelope encryption for OAuth refresh tokens (TASK-011).
//!
//! Mirrors `privacy::EnvelopeCrypto`'s wrap-DEK-with-a-KEK shape, but keyed by
//! tenant rather than data subject: a subject's erasure must never be able to
//! reach a live ad-account credential, and the platform's own KEK rotation
//! schedule (not a person's DSR) governs when this key turns over.
//!
//! `advertising.connections.token_ref` stores only the connection's own id —
//! an opaque locator, never a credential. `TokenVault` resolves that locator
//! to the encrypted refresh token and back, through the same tenant-scoped
//! transaction the caller already holds.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use super::{AdvertisingError, SecretRef};
use sanvi_persistence::ScopedTx;

#[derive(Debug, Clone)]
pub struct TenantKeyRow {
    pub wrapped_dek: Vec<u8>,
    pub kek_id: String,
}

/// Storage for the wrapped per-tenant DEK. `get_or_create` is an atomic
/// upsert-and-return so concurrent first-connections for the same tenant
/// converge on one canonical DEK rather than racing.
#[async_trait]
pub trait TenantKeyRepository: Send + Sync {
    async fn get_or_create(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        candidate_wrapped_dek: Vec<u8>,
        kek_id: &str,
    ) -> Result<TenantKeyRow, AdvertisingError>;
}

/// Storage for the encrypted refresh token, one row per connection.
#[async_trait]
pub trait ConnectionTokenRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        connection_id: Uuid,
        ciphertext: Vec<u8>,
    ) -> Result<(), AdvertisingError>;
    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        connection_id: Uuid,
    ) -> Result<Option<Vec<u8>>, AdvertisingError>;
}

/// AES-256-GCM encrypt/decrypt with the tenant's DEK. No method reads
/// ambient tenant state — the caller's transaction scope carries it.
#[async_trait]
pub trait TenantEnvelopeCrypto: Send + Sync {
    async fn encrypt_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, AdvertisingError>;
    async fn decrypt_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, AdvertisingError>;
}

/// Turns a raw refresh token into an opaque `SecretRef` and back. The
/// locator is the connection's own id — nothing token-shaped is ever
/// returned to a caller outside this module.
#[derive(Clone)]
pub struct TokenVault {
    crypto: Arc<dyn TenantEnvelopeCrypto>,
    tokens: Arc<dyn ConnectionTokenRepository>,
}

fn validate_refresh_token(refresh_token: &str) -> Result<(), AdvertisingError> {
    if refresh_token.trim().is_empty() {
        return Err(AdvertisingError::InvalidInput(
            "advertising refresh token is required".to_string(),
        ));
    }
    Ok(())
}

impl TokenVault {
    pub fn new(
        crypto: Arc<dyn TenantEnvelopeCrypto>,
        tokens: Arc<dyn ConnectionTokenRepository>,
    ) -> Self {
        Self { crypto, tokens }
    }

    pub async fn store_refresh_token(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        connection_id: Uuid,
        refresh_token: &str,
    ) -> Result<SecretRef, AdvertisingError> {
        validate_refresh_token(refresh_token)?;
        let ciphertext = self
            .crypto
            .encrypt_for_tenant(scope, tenant_id, refresh_token.as_bytes())
            .await?;
        self.tokens
            .upsert(scope, tenant_id, connection_id, ciphertext)
            .await?;
        Ok(SecretRef::new(connection_id.to_string()))
    }

    pub async fn resolve_refresh_token(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        connection_id: Uuid,
    ) -> Result<String, AdvertisingError> {
        let ciphertext = self
            .tokens
            .find(scope, tenant_id, connection_id)
            .await?
            .ok_or_else(|| {
                AdvertisingError::Storage("advertising connection token missing".to_string())
            })?;
        let plaintext = self
            .crypto
            .decrypt_for_tenant(scope, tenant_id, &ciphertext)
            .await?;
        String::from_utf8(plaintext).map_err(|_| {
            AdvertisingError::EncryptionFailure("decrypted token is not valid UTF-8".to_string())
        })
    }
}

// `TokenVault`'s I/O paths take a real `ScopedTx`, which wraps a live
// database transaction and cannot be constructed without a database — the
// same reason `privacy`'s envelope-crypto adapter unit-tests only its sync
// wrap/unwrap and leaves the DEK-fetching paths to integration coverage.
// `adapters::token_vault` carries the equivalent sync AES-GCM tests; the
// full store/resolve round trip is covered by the testcontainers suite in
// `tests/advertising_integration.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_blank_refresh_token_is_rejected() {
        assert!(matches!(
            validate_refresh_token("  "),
            Err(AdvertisingError::InvalidInput(_))
        ));
        assert!(validate_refresh_token("1//refresh-token").is_ok());
    }
}
