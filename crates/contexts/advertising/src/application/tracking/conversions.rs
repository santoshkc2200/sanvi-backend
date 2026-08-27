use std::sync::Arc;

use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::domain::{
    AdvertisingError, ConversionDiagnostics, ConversionEvent, ConversionEventRepository,
};

/// Diagnostics read model backing `GET /ads/conversions` — what was
/// captured, and what the gate decided, for a tenant to inspect. No
/// upload path reads from here; TASK-015 owns that.
pub struct ListConversions {
    tx: TxManager,
    repo: Arc<dyn ConversionEventRepository>,
}

impl ListConversions {
    pub fn new(tx: TxManager, repo: Arc<dyn ConversionEventRepository>) -> Self {
        Self { tx, repo }
    }

    pub async fn execute(&self, tenant_id: Uuid) -> Result<Vec<ConversionEvent>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.repo.list(&mut scope, tenant_id).await
    }
}

/// Reads a single conversion event by id and tenant, returning its diagnostics view.
pub struct GetConversionDiagnostics {
    tx: TxManager,
    repo: Arc<dyn ConversionEventRepository>,
}

impl GetConversionDiagnostics {
    pub fn new(tx: TxManager, repo: Arc<dyn ConversionEventRepository>) -> Self {
        Self { tx, repo }
    }

    pub async fn execute(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<ConversionDiagnostics, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let event = self
            .repo
            .find_by_id(&mut scope, tenant_id, id)
            .await?
            .ok_or(AdvertisingError::ConversionNotFound)?;
        let captured_at = event.occurred_at;
        Ok(ConversionDiagnostics { event, captured_at })
    }
}
