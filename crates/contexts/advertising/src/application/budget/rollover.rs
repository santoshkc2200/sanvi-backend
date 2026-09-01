//! Budget period rollover sweep: automatically resumes guardrail-paused campaigns
//! if (and only if) `auto_resume_on_rollover` is enabled (TASK-017, slice 10.8).

use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::{PlatformScope, TxManager};
use uuid::Uuid;

use crate::domain::model::{BudgetPeriod, CampaignStatus};
use crate::domain::{AdvertisingError, BudgetCapRepository, CampaignRepository};

pub struct BudgetRolloverSweep {
    tx: TxManager,
    caps: Arc<dyn BudgetCapRepository>,
    campaigns: Arc<dyn CampaignRepository>,
    resume_campaign: Arc<crate::application::campaigns::ResumeCampaign>,
}

impl BudgetRolloverSweep {
    pub fn new(
        tx: TxManager,
        caps: Arc<dyn BudgetCapRepository>,
        campaigns: Arc<dyn CampaignRepository>,
        resume_campaign: Arc<crate::application::campaigns::ResumeCampaign>,
    ) -> Self {
        Self {
            tx,
            caps,
            campaigns,
            resume_campaign,
        }
    }

    pub async fn sweep_tenant(
        &self,
        tenant_id: TenantId,
        period: BudgetPeriod,
    ) -> Result<usize, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let caps_list = self.caps.list_by_tenant(&mut scope, tenant_id).await?;
        let stored_campaigns = self.campaigns.list_by_tenant(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let tenant_cap = caps_list
            .iter()
            .find(|c| c.campaign_id.is_none() && c.period == period);

        let mut resumed_count = 0;

        for campaign in stored_campaigns {
            if campaign.intent.status != CampaignStatus::Paused || campaign.external_id.is_none() {
                continue;
            }

            let camp_cap = caps_list
                .iter()
                .find(|c| c.campaign_id == Some(campaign.id) && c.period == period);

            let should_resume = match (camp_cap, tenant_cap) {
                (Some(cc), _) => cc.auto_resume_on_rollover,
                (None, Some(tc)) => tc.auto_resume_on_rollover,
                (None, None) => false,
            };

            if should_resume {
                let mutation_id = Uuid::new_v4();
                if self
                    .resume_campaign
                    .execute_guardrail_rollover(
                        tenant_id,
                        campaign.id,
                        mutation_id,
                        "system:budget_guardrail_rollover",
                    )
                    .await
                    .is_ok()
                {
                    resumed_count += 1;
                }
            }
        }

        Ok(resumed_count)
    }

    pub async fn sweep_all(
        &self,
        scope: &mut PlatformScope<'_>,
        period: BudgetPeriod,
    ) -> Result<usize, AdvertisingError> {
        let caps = self.caps.list_all_active_caps(scope).await?;
        let mut tenants: Vec<TenantId> = caps.into_iter().map(|c| c.tenant_id).collect();
        tenants.sort_unstable();
        tenants.dedup();

        let mut total_resumed = 0;
        for tenant_id in tenants {
            let count = self.sweep_tenant(tenant_id, period).await.unwrap_or(0);
            total_resumed += count;
        }

        Ok(total_resumed)
    }
}
