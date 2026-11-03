//! Sqlx adapter for the tenant's published-store binding (the phase-14
//! host → store decision). One row per tenant, under the canonical
//! `tenant_isolation` RLS policy like every other tenant-scoped table:
//! both paths know the tenant before they touch the table — the shopper
//! read resolves host → tenant first, the staff publish runs inside the
//! authenticated tenant session — so every access opens a scoped
//! transaction (`SET LOCAL sanvi.tenant_id`). The store of record lives
//! in ecommerce-service, so validity is re-checked there, not here.

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
        let mut tx = self
            .0
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| ProductAccessError::Storage(err.to_string()))?;
        let row = sqlx::query!(
            "SELECT store_id FROM platform.ecommerce_public_store_bindings WHERE tenant_id = $1",
            tenant_id.as_i64()
        )
        .fetch_optional(&mut **tx.tx_mut().as_inner())
        .await
        .map_err(|err| ProductAccessError::Storage(err.to_string()))?;
        tx.commit()
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
            .begin_for(tenant_id.as_i64())
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
            .execute(&mut **tx.tx_mut().as_inner())
            .await
            .map_err(|err| ProductAccessError::Storage(err.to_string()))?;
        } else {
            sqlx::query!(
                "DELETE FROM platform.ecommerce_public_store_bindings WHERE tenant_id = $1",
                tenant_id.as_i64()
            )
            .execute(&mut **tx.tx_mut().as_inner())
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
