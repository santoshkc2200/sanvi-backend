//! Reconciliation use case.  It has no provider write capability by design.

use std::sync::Arc;

use crate::domain::model::CampaignId;
use chrono::Utc;
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::domain::{
    AdEvent, AdvertisingError, Campaign, CampaignChange, CampaignReconciliationState,
    CampaignRepository, ChangeSource, EventPublisher, ReconciliationResult, StoredCampaign,
};

pub struct ReconcileCampaign {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl ReconcileCampaign {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            events,
            ids,
        }
    }

    /// Store a provider observation. This operation never calls the provider,
    /// so a reconciliation pass cannot resurrect a paused remote campaign.
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        remote: Campaign,
    ) -> Result<ReconciliationResult, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let stored = self
            .campaigns
            .find(&mut scope, tenant_id, campaign_id)
            .await?
            .ok_or(AdvertisingError::CampaignNotFound)?;
        let before_remote = stored.remote_snapshot.clone();
        let mut state = CampaignReconciliationState {
            intent: stored.intent,
            last_known_remote: stored.remote_snapshot,
        };
        let result = state.reconcile(remote);
        if result == ReconciliationResult::Unchanged {
            scope
                .rollback()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            return Ok(result);
        }

        let updated = StoredCampaign {
            intent: state.intent.clone(),
            remote_snapshot: state.last_known_remote,
            revision: stored.revision + 1,
            ..stored
        };
        if !self
            .campaigns
            .update(&mut scope, &updated, updated.revision - 1)
            .await?
        {
            return Err(AdvertisingError::StaleRevision);
        }
        let changed_fields = match &result {
            ReconciliationResult::RemoteUpdated { remote_changes } => remote_changes.clone(),
            ReconciliationResult::Drifted { fields } => fields.clone(),
            ReconciliationResult::Unchanged => unreachable!(),
        };
        self.campaigns
            .append_change(
                &mut scope,
                &CampaignChange {
                    id: self.ids.next_campaign_change_id()?,
                    tenant_id,
                    campaign_id,
                    revision: updated.revision,
                    mutation_id: None,
                    actor_id: None,
                    source: ChangeSource::Platform,
                    changed_fields: changed_fields.clone(),
                    before_state: before_remote,
                    after_state: updated.remote_snapshot.clone(),
                    created_at: Utc::now(),
                },
            )
            .await?;
        if let ReconciliationResult::Drifted { .. } = &result {
            self.events
                .publish_scoped(
                    &mut scope,
                    tenant_id,
                    &[AdEvent::CampaignDrifted {
                        campaign_id,
                        changed_fields,
                    }],
                )
                .await?;
        }
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(result)
    }
}
