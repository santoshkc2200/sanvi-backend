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

        let updated = self
            .alerts
            .acknowledge_alert(&mut scope, tenant_id, alert_id, user_id, now)
            .await?
            .ok_or(AdvertisingError::BudgetAlertNotFound)?;

        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        Ok(updated)
    }
}
