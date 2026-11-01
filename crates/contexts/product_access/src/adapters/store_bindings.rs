//! Sqlx adapter for the tenant's published-store binding (the phase-14
//! host → store decision). Platform-scoped: one row per tenant, no RLS —
//! the tenant id is the primary key and the store of record lives in
//! ecommerce-service, so validity is re-checked there, not here.

use async_trait::async_trait;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::{ProductAccessError, StoreBindings};

pub struct SqlxStoreBindings(TxManager);

impl SqlxStoreBindings {
    pub fn new(tx: TxManager) -> Self {
        Self(tx)
    }
}

#[async_trait]
impl StoreBindings for SqlxStoreBindings {
    async fn published_store(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<i64>, ProductAccessError> {
        let row = sqlx::query!(
            "SELECT store_id FROM platform.ecommerce_public_store_bindings WHERE tenant_id = $1",
            tenant_id.as_i64()
        )
        .fetch_optional(self.0.pool())
        .await
        .map_err(|err| ProductAccessError::Storage(err.to_string()))?;
        Ok(row.map(|record| record.store_id))
    }

    async fn publish(
        &self,
        tenant_id: TenantId,
        store_id: Option<i64>,
    ) -> Result<(), ProductAccessError> {
        let mut tx = self
            .0
            .begin()
            .await
            .map_err(|err| ProductAccessError::Storage(err.to_string()))?;
        if let Some(store_id) = store_id {
            sqlx::query!(
                "INSERT INTO platform.ecommerce_public_store_bindings (tenant_id, store_id) \
                 VALUES ($1, $2) \
                 ON CONFLICT (tenant_id) DO UPDATE SET store_id = EXCLUDED.store_id, updated_at = now()",
                tenant_id.as_i64(),
                store_id
            )
            .execute(&mut **tx.as_inner())
            .await
            .map_err(|err| ProductAccessError::Storage(err.to_string()))?;
        } else {
            sqlx::query!(
                "DELETE FROM platform.ecommerce_public_store_bindings WHERE tenant_id = $1",
                tenant_id.as_i64()
            )
            .execute(&mut **tx.as_inner())
            .await
            .map_err(|err| ProductAccessError::Storage(err.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|err| ProductAccessError::Storage(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_ids_travel_as_i64() {
        let tenant = TenantId::try_from(1001_i64).unwrap();
        assert_eq!(tenant.as_i64(), 1001);
    }
}
