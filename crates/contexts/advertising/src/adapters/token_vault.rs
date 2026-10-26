//! SQLx persistence and AES-256-GCM envelope crypto for `TokenVault`
//! (TASK-011). Same construction as `privacy::SqlxEnvelopeCrypto` — a random
//! DEK wrapped by a KEK held only in configuration — keyed by tenant instead
//! of data subject.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use sanvi_kernel::TenantId;
use sanvi_persistence::ScopedTx;

use crate::domain::model::ConnectionId;

use crate::domain::{
    AdvertisingError, ConnectionTokenRepository, TenantEnvelopeCrypto, TenantKeyRepository,
    TenantKeyRow,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxTenantKeyRepository;

impl SqlxTenantKeyRepository {
    pub const fn new() -> Self {
        Self
    }
}

fn storage(error: sqlx::Error) -> AdvertisingError {
    AdvertisingError::Storage(error.to_string())
}

#[async_trait]
impl TenantKeyRepository for SqlxTenantKeyRepository {
    async fn get_or_create(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        candidate_wrapped_dek: Vec<u8>,
        kek_id: &str,
    ) -> Result<TenantKeyRow, AdvertisingError> {
        // Upsert-and-return-existing in one round trip: a no-op DO UPDATE on
        // conflict means the RETURNING clause always yields the canonical
        // row, whether this call created it or a concurrent one did.
        let row: (Vec<u8>, String) = sqlx::query_as(
            "INSERT INTO advertising.tenant_secret_keys (tenant_id, wrapped_dek, kek_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id) DO UPDATE SET tenant_id = EXCLUDED.tenant_id
             RETURNING wrapped_dek, kek_id",
        )
        .bind(tenant_id.as_i64())
        .bind(&candidate_wrapped_dek)
        .bind(kek_id)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(TenantKeyRow {
            wrapped_dek: row.0,
            kek_id: row.1,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxConnectionTokenRepository;

impl SqlxConnectionTokenRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ConnectionTokenRepository for SqlxConnectionTokenRepository {
    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        connection_id: ConnectionId,
        ciphertext: Vec<u8>,
    ) -> Result<(), AdvertisingError> {
        sqlx::query(
            "INSERT INTO advertising.connection_tokens (connection_id, tenant_id, ciphertext)
             VALUES ($1, $2, $3)
             ON CONFLICT (connection_id) DO UPDATE SET
               ciphertext = EXCLUDED.ciphertext, tenant_id = EXCLUDED.tenant_id, updated_at = now()",
        )
        .bind(connection_id.as_i64())
        .bind(tenant_id.as_i64())
        .bind(ciphertext)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        connection_id: ConnectionId,
    ) -> Result<Option<Vec<u8>>, AdvertisingError> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT ciphertext FROM advertising.connection_tokens
             WHERE tenant_id = $1 AND connection_id = $2",
        )
        .bind(tenant_id.as_i64())
        .bind(connection_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(row.map(|(ciphertext,)| ciphertext))
    }
}

/// The concrete `TenantEnvelopeCrypto`: AES-256-GCM, per-tenant DEKs wrapped
/// in `advertising.tenant_secret_keys`, a KEK from configuration.
pub struct SqlxTenantEnvelopeCrypto {
    keys: SqlxTenantKeyRepository,
    kek: [u8; 32],
    kek_id: String,
    dek_cache: Mutex<HashMap<TenantId, [u8; 32]>>,
}

impl SqlxTenantEnvelopeCrypto {
    /// Rewrap without changing token ciphertext. Persist with the new KEK id
    /// atomically, then restart cached-DEK users before retiring the old KEK.
    pub fn rewrap_dek(
        &self,
        wrapped: &[u8],
        replacement: &Self,
    ) -> Result<Vec<u8>, AdvertisingError> {
        replacement.wrap(&self.unwrap(wrapped)?)
    }

    /// `kek` must be exactly 32 bytes (validated by `AdvertisingConfig`).
    pub fn new(kek: [u8; 32], kek_id: String) -> Self {
        Self {
            keys: SqlxTenantKeyRepository::new(),
            kek,
            kek_id,
            dek_cache: Mutex::new(HashMap::new()),
        }
    }

    fn wrap(&self, dek: &[u8; 32]) -> Result<Vec<u8>, AdvertisingError> {
        sanvi_crypto::seal(&self.kek, dek)
            .map_err(|_| AdvertisingError::EncryptionFailure("DEK wrap failed".to_string()))
    }

    fn unwrap(&self, wrapped: &[u8]) -> Result<[u8; 32], AdvertisingError> {
        sanvi_crypto::open(&self.kek, wrapped)
            .map_err(|_| AdvertisingError::EncryptionFailure("DEK unwrap failed".to_string()))?
            .try_into()
            .map_err(|_| AdvertisingError::EncryptionFailure("DEK must be 32 bytes".to_string()))
    }

    async fn fetch_dek(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<[u8; 32], AdvertisingError> {
        if let Ok(cache) = self.dek_cache.lock()
            && let Some(dek) = cache.get(&tenant_id)
        {
            return Ok(*dek);
        }
        let candidate = sanvi_crypto::generate_key();
        let candidate_wrapped = self.wrap(&candidate)?;
        let row = self
            .keys
            .get_or_create(scope, tenant_id, candidate_wrapped, &self.kek_id)
            .await?;
        if row.kek_id != self.kek_id {
            return Err(AdvertisingError::EncryptionFailure(format!(
                "tenant DEK wrapped with unknown KEK id {:?}",
                row.kek_id
            )));
        }
        let dek = self.unwrap(&row.wrapped_dek)?;
        if let Ok(mut cache) = self.dek_cache.lock() {
            if cache.len() > 10_000 {
                cache.clear();
            }
            cache.insert(tenant_id, dek);
        }
        Ok(dek)
    }
}

#[async_trait]
impl TenantEnvelopeCrypto for SqlxTenantEnvelopeCrypto {
    async fn encrypt_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, AdvertisingError> {
        let dek = self.fetch_dek(scope, tenant_id).await?;
        sanvi_crypto::seal(&dek, plaintext)
            .map_err(|_| AdvertisingError::EncryptionFailure("encrypt failed".to_string()))
    }

    async fn decrypt_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, AdvertisingError> {
        if ciphertext.len() < 13 {
            return Err(AdvertisingError::EncryptionFailure(
                "ciphertext too short".to_string(),
            ));
        }
        let dek = self.fetch_dek(scope, tenant_id).await?;
        sanvi_crypto::open(&dek, ciphertext)
            .map_err(|_| AdvertisingError::EncryptionFailure("decrypt failed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crypto() -> SqlxTenantEnvelopeCrypto {
        SqlxTenantEnvelopeCrypto::new([7u8; 32], "test-kek".to_string())
    }

    #[test]
    fn wrap_unwrap_roundtrips() {
        let crypto = crypto();
        let dek = [3u8; 32];
        let wrapped = crypto.wrap(&dek).unwrap();
        let unwrapped = crypto.unwrap(&wrapped).unwrap();
        assert_eq!(unwrapped, dek);
    }

    #[test]
    fn rewrap_preserves_dek_and_retires_old_kek() {
        let old = crypto();
        let new = SqlxTenantEnvelopeCrypto::new([8; 32], "replacement".into());
        let dek = [3; 32];
        let wrapped = old.wrap(&dek).unwrap();
        let rewrapped = old.rewrap_dek(&wrapped, &new).unwrap();
        assert_eq!(new.unwrap(&rewrapped).unwrap(), dek);
        assert!(old.unwrap(&rewrapped).is_err());
        assert!(new.rewrap_dek(&wrapped, &old).is_err());
        assert!(old.rewrap_dek(&[0; 5], &new).is_err());
    }

    #[test]
    fn unwrap_rejects_a_ciphertext_wrapped_by_a_different_kek() {
        let a = SqlxTenantEnvelopeCrypto::new([1u8; 32], "kek-a".to_string());
        let b = SqlxTenantEnvelopeCrypto::new([2u8; 32], "kek-b".to_string());
        let wrapped = a.wrap(&[9u8; 32]).unwrap();
        assert!(b.unwrap(&wrapped).is_err());
    }

    #[test]
    fn unwrap_rejects_a_truncated_wrapped_key() {
        let crypto = crypto();
        assert!(crypto.unwrap(&[0u8; 5]).is_err());
    }
}
