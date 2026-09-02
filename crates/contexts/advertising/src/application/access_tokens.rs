//! Vault-backed `ConnectionAccessTokenProvider` (TASK-012, slice 10.2).
//!
//! An adapter is shared across tenants and holds no transaction, so it cannot
//! reach the tenant-scoped `TokenVault` itself. This provider closes that gap:
//! it opens the tenant's own scope, decrypts the stored refresh token, and
//! trades it for a short-lived access token through a platform-specific
//! [`RefreshTokenGrant`]. The refresh token never leaves this module, and the
//! tenant comes from the caller's `ConnectionHandle` rather than a
//! cross-tenant lookup, so RLS still bounds every read.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::model::ConnectionId;
use crate::domain::{
    AdvertisingError, ConnectionAccessTokenProvider, ConnectionHandle, RefreshTokenGrant,
    TokenVault,
};

/// Discard a cached token this long before the platform would expire it, so a
/// request in flight cannot be the one that discovers the expiry.
const EXPIRY_MARGIN_SECONDS: i64 = 60;

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Option<DateTime<Utc>>,
}

impl CachedToken {
    fn usable_at(&self, now: DateTime<Utc>) -> bool {
        match self.expires_at {
            // No expiry advertised: treat as single-use rather than immortal.
            None => false,
            Some(expires_at) => now + Duration::seconds(EXPIRY_MARGIN_SECONDS) < expires_at,
        }
    }
}

pub struct VaultAccessTokenProvider {
    tx: TxManager,
    tokens: Arc<TokenVault>,
    grant: Arc<dyn RefreshTokenGrant>,
    cache: Mutex<HashMap<(i64, ConnectionId), CachedToken>>,
}

impl VaultAccessTokenProvider {
    pub fn new(tx: TxManager, tokens: Arc<TokenVault>, grant: Arc<dyn RefreshTokenGrant>) -> Self {
        Self {
            tx,
            tokens,
            grant,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, key: &(i64, ConnectionId), now: DateTime<Utc>) -> Option<String> {
        let cache = self.cache.lock().ok()?;
        cache
            .get(key)
            .filter(|entry| entry.usable_at(now))
            .map(|entry| entry.access_token.clone())
    }

    fn remember(&self, key: (i64, ConnectionId), token: CachedToken) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key, token);
        }
    }

    async fn refresh_token_for(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectionId,
    ) -> Result<String, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let refresh_token = self
            .tokens
            .resolve_refresh_token(&mut scope, tenant_id, connection_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(refresh_token)
    }
}

#[async_trait]
impl ConnectionAccessTokenProvider for VaultAccessTokenProvider {
    async fn access_token(
        &self,
        connection: &ConnectionHandle,
    ) -> Result<String, AdvertisingError> {
        let connection_id = connection.id.parse::<ConnectionId>().map_err(|_| {
            AdvertisingError::InvalidInput("connection handle is not a connection id".to_string())
        })?;
        let tenant_id = connection.tenant_id;
        let key = (tenant_id.as_i64(), connection_id);
        let now = Utc::now();

        if let Some(token) = self.cached(&key, now) {
            return Ok(token);
        }

        let refresh_token = self.refresh_token_for(tenant_id, connection_id).await?;
        let granted = self.grant.exchange_refresh_token(&refresh_token).await?;
        self.remember(
            key,
            CachedToken {
                access_token: granted.access_token.clone(),
                expires_at: granted.expires_at,
            },
        );
        Ok(granted.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(expires_in: Option<i64>, now: DateTime<Utc>) -> CachedToken {
        CachedToken {
            access_token: "ya29.cached".to_string(),
            expires_at: expires_in.map(|secs| now + Duration::seconds(secs)),
        }
    }

    #[test]
    fn a_token_well_inside_its_lifetime_is_reused() {
        let now = Utc::now();
        assert!(cached(Some(3600), now).usable_at(now));
    }

    #[test]
    fn a_token_inside_the_expiry_margin_is_discarded() {
        let now = Utc::now();
        // Still nominally valid, but not for long enough to survive a request.
        assert!(!cached(Some(EXPIRY_MARGIN_SECONDS - 1), now).usable_at(now));
    }

    #[test]
    fn an_expired_token_is_discarded() {
        let now = Utc::now();
        assert!(!cached(Some(-1), now).usable_at(now));
    }

    #[test]
    fn a_token_with_no_advertised_expiry_is_never_reused() {
        let now = Utc::now();
        assert!(!cached(None, now).usable_at(now));
    }
}
