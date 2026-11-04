//! Persist allocations before calling ecommerce-service. No contact or address
//! data is stored. The platform's 24-hour replay window exceeds the hold TTL.
use crate::domain::{CheckoutAllocation, CheckoutReservations, ShopperError};
use async_trait::async_trait;
use sanvi_kernel::TenantId;
use sanvi_persistence::{IdempotencyHit, IdempotencyStore};
use std::time::Duration;

pub struct StoredCheckoutReservations(IdempotencyStore);
impl StoredCheckoutReservations {
    pub fn new(store: IdempotencyStore) -> Self {
        Self(store)
    }
}
fn scope(tenant: TenantId, store: i64) -> String {
    format!("tenant.{tenant}.ecommerce.store.{store}.checkout-allocation")
}
fn decode(hit: IdempotencyHit) -> Result<Vec<CheckoutAllocation>, ShopperError> {
    let value = hit.response.ok_or_else(|| {
        ShopperError::Unavailable(
            "checkout allocation is being recorded; retry the same key".to_owned(),
        )
    })?;
    serde_json::from_value(value).map_err(|e| ShopperError::Storage(e.to_string()))
}
#[async_trait]
impl CheckoutReservations for StoredCheckoutReservations {
    async fn load(
        &self,
        tenant: TenantId,
        store: i64,
        key: &str,
    ) -> Result<Option<Vec<CheckoutAllocation>>, ShopperError> {
        self.0
            .lookup(key, &scope(tenant, store))
            .await
            .map_err(|e| ShopperError::Storage(e.to_string()))?
            .map(decode)
            .transpose()
    }
    async fn remember(
        &self,
        tenant: TenantId,
        store: i64,
        key: &str,
        lines: Vec<CheckoutAllocation>,
    ) -> Result<Vec<CheckoutAllocation>, ShopperError> {
        let scope = scope(tenant, store);
        // A crash before resolve cannot have created an upstream hold. The
        // unfinished claim expires quickly, allowing the shopper to retry.
        if let Some(hit) = self
            .0
            .claim(key, &scope, Duration::from_secs(30))
            .await
            .map_err(|e| ShopperError::Storage(e.to_string()))?
        {
            return decode(hit);
        }
        let value =
            serde_json::to_value(&lines).map_err(|e| ShopperError::Storage(e.to_string()))?;
        self.0
            .resolve(key, &scope, 200, value)
            .await
            .map_err(|e| ShopperError::Storage(e.to_string()))?;
        Ok(lines)
    }
}
