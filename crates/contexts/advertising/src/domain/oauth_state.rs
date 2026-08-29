//! Signed, tenant-bound OAuth state with single-use redemption.
//!
//! The signed payload lets a callback fail closed before any provider token is
//! exchanged. The server-side nonce registry makes a valid state unusable after
//! one callback, including when a browser or provider retries a redirect.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use super::PlatformKey;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthStateClaims {
    pub tenant_id: TenantId,
    pub platform: PlatformKey,
    pub nonce: Uuid,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OAuthStateError {
    #[error("OAuth state is missing or malformed")]
    Malformed,
    #[error("OAuth state signature is invalid")]
    Forged,
    #[error("OAuth state has expired")]
    Expired,
    #[error("OAuth state belongs to another tenant or platform")]
    CrossTenant,
    #[error("OAuth state was already redeemed or was never issued")]
    Replayed,
    #[error("OAuth state store is unavailable")]
    StoreUnavailable,
}

impl From<OAuthStateError> for super::AdvertisingError {
    fn from(error: OAuthStateError) -> Self {
        match error {
            OAuthStateError::StoreUnavailable => Self::Storage(error.to_string()),
            _ => Self::InvalidInput(error.to_string()),
        }
    }
}

/// Durable, single-use nonce storage. Implementations must atomically consume
/// a nonce only when all signed claims still match. The signature is verified
/// before this port is reached, so a callback can never turn the store into an
/// oracle for arbitrary nonce values.
#[async_trait]
pub trait OAuthStateStore: Send + Sync {
    async fn issue(&self, claims: OAuthStateClaims) -> Result<(), OAuthStateError>;
    async fn consume(&self, claims: &OAuthStateClaims) -> Result<bool, OAuthStateError>;
}

#[derive(Default)]
pub struct InMemoryOAuthStateStore {
    issued: Mutex<BTreeMap<Uuid, OAuthStateClaims>>,
}

#[async_trait]
impl OAuthStateStore for InMemoryOAuthStateStore {
    async fn issue(&self, claims: OAuthStateClaims) -> Result<(), OAuthStateError> {
        self.issued
            .lock()
            .map_err(|_| OAuthStateError::StoreUnavailable)?
            .insert(claims.nonce, claims);
        Ok(())
    }

    async fn consume(&self, claims: &OAuthStateClaims) -> Result<bool, OAuthStateError> {
        Ok(self
            .issued
            .lock()
            .map_err(|_| OAuthStateError::StoreUnavailable)?
            .remove(&claims.nonce)
            .is_some_and(|stored| stored == *claims))
    }
}

/// The nonce store is intentionally separate from the signature. Production
/// wiring replaces this process-local implementation with the repository in
/// TASK-011's connection adapter; keeping the issue/redeem protocol here
/// prevents an adapter from weakening tenant binding or replay protection.
#[derive(Clone)]
pub struct OAuthStateManager {
    signing_key: Arc<[u8]>,
    store: Arc<dyn OAuthStateStore>,
}

impl OAuthStateManager {
    pub fn new(signing_key: impl AsRef<[u8]>) -> Self {
        Self {
            signing_key: Arc::from(signing_key.as_ref()),
            store: Arc::new(InMemoryOAuthStateStore::default()),
        }
    }

    pub fn with_store(signing_key: impl AsRef<[u8]>, store: Arc<dyn OAuthStateStore>) -> Self {
        Self {
            signing_key: Arc::from(signing_key.as_ref()),
            store,
        }
    }

    /// Issue an opaque state string. Its body includes no authorization code or
    /// token, so it is safe to carry through browser redirects.
    pub async fn issue(
        &self,
        tenant_id: TenantId,
        platform: PlatformKey,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> Result<String, OAuthStateError> {
        let claims = OAuthStateClaims {
            tenant_id,
            platform,
            nonce: Uuid::now_v7(),
            expires_at: now + ttl,
        };
        self.store.issue(claims.clone()).await?;
        Ok(self.sign(&claims))
    }

    /// Verify signature, expiry, tenant/platform binding, then atomically burn
    /// state. A caller must redeem before exchanging the provider's code.
    pub async fn redeem(
        &self,
        state: &str,
        expected_tenant_id: TenantId,
        expected_platform: &PlatformKey,
        now: DateTime<Utc>,
    ) -> Result<OAuthStateClaims, OAuthStateError> {
        let claims = self.verify(state)?;
        if claims.tenant_id != expected_tenant_id || &claims.platform != expected_platform {
            return Err(OAuthStateError::CrossTenant);
        }
        if claims.expires_at <= now {
            return Err(OAuthStateError::Expired);
        }
        if !self.store.consume(&claims).await? {
            return Err(OAuthStateError::Replayed);
        }
        Ok(claims)
    }

    fn sign(&self, claims: &OAuthStateClaims) -> String {
        let payload = serde_json::to_vec(claims).expect("OAuth state claims serialize");
        let encoded = URL_SAFE_NO_PAD.encode(payload);
        let mut mac =
            HmacSha256::new_from_slice(&self.signing_key).expect("HMAC accepts all key sizes");
        mac.update(encoded.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("v1.{encoded}.{signature}")
    }

    fn verify(&self, state: &str) -> Result<OAuthStateClaims, OAuthStateError> {
        let mut parts = state.split('.');
        let (version, payload, signature) = (parts.next(), parts.next(), parts.next());
        if version != Some("v1")
            || payload.is_none()
            || signature.is_none()
            || parts.next().is_some()
        {
            return Err(OAuthStateError::Malformed);
        }
        let payload = payload.expect("checked above");
        let signature = URL_SAFE_NO_PAD
            .decode(signature.expect("checked above"))
            .map_err(|_| OAuthStateError::Malformed)?;
        let mut mac =
            HmacSha256::new_from_slice(&self.signing_key).expect("HMAC accepts all key sizes");
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| OAuthStateError::Forged)?;
        let raw = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| OAuthStateError::Malformed)?;
        serde_json::from_slice(&raw).map_err(|_| OAuthStateError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn manager() -> OAuthStateManager {
        OAuthStateManager::new(b"test-oauth-state-key")
    }
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 24, 0, 0, 0).unwrap()
    }
    fn platform() -> PlatformKey {
        PlatformKey::parse("test_platform").unwrap()
    }

    fn tenant_id(n: i64) -> TenantId {
        TenantId::try_from(n).unwrap()
    }

    #[tokio::test]
    async fn signed_state_rejects_forgery_expiry_replay_and_cross_tenant_redemption() {
        let manager = manager();
        let tenant_a = tenant_id(1_000_001);
        let tenant_b = tenant_id(1_000_002);
        let state = manager
            .issue(tenant_a, platform(), now(), Duration::minutes(10))
            .await
            .unwrap();
        assert_eq!(
            manager.redeem(&state, tenant_b, &platform(), now()).await,
            Err(OAuthStateError::CrossTenant)
        );
        assert!(
            manager
                .redeem(&state, tenant_a, &platform(), now())
                .await
                .is_ok()
        );
        assert_eq!(
            manager.redeem(&state, tenant_a, &platform(), now()).await,
            Err(OAuthStateError::Replayed)
        );

        let forged = format!("{state}x");
        assert_eq!(
            manager.redeem(&forged, tenant_a, &platform(), now()).await,
            Err(OAuthStateError::Forged)
        );

        let expired = manager
            .issue(tenant_a, platform(), now(), Duration::seconds(-1))
            .await
            .unwrap();
        assert_eq!(
            manager.redeem(&expired, tenant_a, &platform(), now()).await,
            Err(OAuthStateError::Expired)
        );
    }
}
