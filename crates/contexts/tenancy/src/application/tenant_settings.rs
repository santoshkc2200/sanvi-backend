//! Tenant-scoped settings: read and update. The scoping guard is structural:
//! the port only accepts `ScopedTx`, so settings can never be read without
//! `SET LOCAL sanvi.tenant_id` in force.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::{Deserialize, Serialize};
use tracing::info;
use utoipa::ToSchema;

use crate::domain::tenant::TenantId;
use crate::domain::{EventPublisher, TenancyError, TenantSettingsRepository, events::TenantEvent};

use super::storage;

/// Settings keys are stable identifiers, not user text.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

pub struct GetTenantSettings {
    tx_manager: TxManager,
    settings: Arc<dyn TenantSettingsRepository>,
}

impl GetTenantSettings {
    pub fn new(tx_manager: TxManager, settings: Arc<dyn TenantSettingsRepository>) -> Self {
        Self {
            tx_manager,
            settings,
        }
    }

    pub async fn execute(&self, tenant_id: TenantId) -> Result<TenantSettingsOutput, TenancyError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_uuid())
            .await
            .map_err(storage)?;
        let entries = self.settings.list_all(&mut scope, tenant_id).await?;
        scope.commit().await.map_err(storage)?;
        Ok(TenantSettingsOutput {
            settings: entries.into_iter().collect(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SettingsUpdate {
    /// Key → value map to upsert. Values are uninterpreted JSON.
    #[schema(example = json!({ "checkout.currency": "JPY" }))]
    pub settings: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct TenantSettingsOutput {
    pub settings: std::collections::BTreeMap<String, serde_json::Value>,
}

pub struct UpdateTenantSettings {
    tx_manager: TxManager,
    settings: Arc<dyn TenantSettingsRepository>,
    events: Arc<dyn EventPublisher>,
}

impl UpdateTenantSettings {
    pub fn new(
        tx_manager: TxManager,
        settings: Arc<dyn TenantSettingsRepository>,
        events: Arc<dyn EventPublisher>,
    ) -> Self {
        Self {
            tx_manager,
            settings,
            events,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        update: SettingsUpdate,
    ) -> Result<TenantSettingsOutput, TenancyError> {
        if update.settings.is_empty() {
            return Err(TenancyError::InvalidSettings(
                "settings must not be empty".to_string(),
            ));
        }
        for key in update.settings.keys() {
            if !valid_key(key) {
                return Err(TenancyError::InvalidSettings(
                    "settings key must match [a-z0-9_.-]{1,64}".to_string(),
                ));
            }
        }

        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_uuid())
            .await
            .map_err(storage)?;

        let mut events = Vec::with_capacity(update.settings.len());
        for (key, value) in &update.settings {
            self.settings
                .set(&mut scope, tenant_id, key, value.clone())
                .await?;
            events.push(TenantEvent::SettingsChanged {
                tenant_id,
                key: key.clone(),
                value: value.clone(),
            });
        }
        self.events.publish_scoped(&mut scope, &events).await?;

        let entries = self.settings.list_all(&mut scope, tenant_id).await?;
        scope.commit().await.map_err(storage)?;

        info!(tenant_id = %tenant_id, keys = events.len(), "tenant settings updated");
        Ok(TenantSettingsOutput {
            settings: entries.into_iter().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation_accepts_stable_identifiers_only() {
        assert!(valid_key("checkout.currency"));
        assert!(valid_key("theme_primary_color-1"));
        assert!(valid_key("a"));
        assert!(!valid_key(""));
        assert!(!valid_key("UPPER"));
        assert!(!valid_key("with space"));
        assert!(!valid_key(&"x".repeat(65)));
    }
}
