use std::collections::BTreeMap;
use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::{AdvertisingError, TrackingSettings, TrackingSettingsRepository};

pub struct ManageTrackingSettings {
    tx: TxManager,
    repo: Arc<dyn TrackingSettingsRepository>,
}

impl ManageTrackingSettings {
    pub fn new(tx: TxManager, repo: Arc<dyn TrackingSettingsRepository>) -> Self {
        Self { tx, repo }
    }

    pub async fn get_settings(
        &self,
        tenant_id: TenantId,
    ) -> Result<TrackingSettings, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        match self.repo.get(&mut scope, tenant_id).await? {
            Some(settings) => Ok(settings),
            None => Ok(TrackingSettings {
                tenant_id,
                mappings: BTreeMap::new(),
            }),
        }
    }

    pub async fn update_settings(
        &self,
        settings: TrackingSettings,
    ) -> Result<(), AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(settings.tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.repo.upsert(&mut scope, &settings).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))
    }
}
