//! Budget alerts listing and acknowledgment.

use std::sync::Arc;

use chrono::Utc;
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;

use crate::domain::model::{BudgetAlert, BudgetAlertId, CampaignId};
use crate::domain::{AdvertisingError, BudgetAlertRepository};

pub struct ListBudgetAlerts {
    tx: TxManager,
    alerts: Arc<dyn BudgetAlertRepository>,
}

impl ListBudgetAlerts {
    pub fn new(tx: TxManager, alerts: Arc<dyn BudgetAlertRepository>) -> Self {
        Self { tx, alerts }
    }

    #[tracing::instrument(name = "advertising.list_budget_alerts", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: Option<CampaignId>,
        unacknowledged_only: bool,
        limit: Option<i64>,
    ) -> Result<Vec<BudgetAlert>, AdvertisingError> {
        let max_limit = limit.unwrap_or(50).clamp(1, 200);
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let result = self
            .alerts
            .list_alerts(
                &mut scope,
                tenant_id,
                campaign_id,
                unacknowledged_only,
                max_limit,
            )
            .await?;

        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        Ok(result)
    }
}

pub struct AcknowledgeBudgetAlert {
    tx: TxManager,
    alerts: Arc<dyn BudgetAlertRepository>,
}

impl AcknowledgeBudgetAlert {
    pub fn new(tx: TxManager, alerts: Arc<dyn BudgetAlertRepository>) -> Self {
        Self { tx, alerts }
    }

    #[tracing::instrument(name = "advertising.acknowledge_budget_alert", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        alert_id: BudgetAlertId,
        user_id: UserId,
    ) -> Result<BudgetAlert, AdvertisingError> {
        let now = Utc::now();
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        // The UPDATE returns None when the alert does not exist OR was already
        // acknowledged (IS NULL guard in the repo). Distinguish the two cases:
        // - alert exists but already acknowledged → return it as a benign no-op
        // - alert not found at all → propagate BudgetAlertNotFound
        let updated = match self
            .alerts
            .acknowledge_alert(&mut scope, tenant_id, alert_id, user_id, now)
            .await?
        {
            Some(alert) => alert,
            None => {
                // Check whether the alert exists (already acknowledged)
                self.alerts
                    .find_by_id(&mut scope, tenant_id, alert_id)
                    .await?
                    .ok_or(AdvertisingError::BudgetAlertNotFound)?
            }
        };

        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        Ok(updated)
    }
}
