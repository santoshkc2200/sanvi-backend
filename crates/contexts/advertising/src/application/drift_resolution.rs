//! Explicit campaign-drift resolution.
//!
//! Reconciliation only observes a provider. This use case is the separate,
//! tenant-attributed write boundary for accepting a native edit or reapplying
//! Sanvi intent. It deliberately has no automatic path.

use std::sync::Arc;

use crate::domain::model::CampaignId;
use chrono::Utc;
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    application::PlatformRegistry,
    domain::{
        AdEvent, AdvertisingConnectionRepository, AdvertisingError, Campaign, CampaignChange,
        CampaignReconciliationState, CampaignRepository, ChangeSource, ConnectionHandle,
        EventPublisher, StoredCampaign,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriftResolution {
    KeepTheirs,
    ReapplyOurs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCampaignDrift {
    pub campaign: Campaign,
    pub revision: i64,
    pub replayed: bool,
}

pub struct ResolveCampaignDrift {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    events: Arc<dyn EventPublisher>,
    platforms: Arc<PlatformRegistry>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl ResolveCampaignDrift {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            events,
            platforms,
            ids,
        }
    }

    /// Resolve one observed drift. `mutation_id` makes both choices safe to
    /// retry after a client timeout; `expected_revision` prevents a stale UI
    /// from resolving a newer drift observation.
    #[tracing::instrument(name = "advertising.resolve_campaign_drift", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        expected_revision: i64,
        mutation_id: Uuid,
        actor_id: UserId,
        resolution: DriftResolution,
    ) -> Result<ResolvedCampaignDrift, AdvertisingError> {
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
        if stored.revision != expected_revision {
            return Err(AdvertisingError::StaleRevision);
        }
        if !stored.intent.drift.drifted {
            return Err(AdvertisingError::InvalidInput(
                "campaign has no unresolved drift".to_string(),
            ));
        }

        let before = stored.intent.clone();
        let changed_fields = before.drift.changed_fields.clone();
        let mut state = CampaignReconciliationState {
            intent: stored.intent,
            last_known_remote: stored.remote_snapshot,
        };
        let event = match resolution {
            DriftResolution::KeepTheirs => {
                state.keep_theirs();
                None
            }
            DriftResolution::ReapplyOurs => {
                let adapter = self.platforms.adapter(&stored.platform)?;
                let remote = adapter
                    .update_campaign(
                        &{
                            let connection = self
                                .connections
                                .find_by_id(&mut scope, tenant_id, stored.connection_id)
                                .await?
                                .ok_or(AdvertisingError::ConnectionNotFound)?;
                            ConnectionHandle {
                                id: connection.id().to_string(),
                                tenant_id: connection.tenant_id(),
                                external_account_id: connection.external_account_id().to_string(),
                            }
                        },
                        state.intent.clone(),
                        mutation_id,
                    )
                    .await?;
                // Provider state remains authoritative. Reconcile its reply
                // before clearing drift: a clamped budget or rejected field
                // must remain visible instead of becoming a false success.
                let _ = state.reconcile(remote);
                Some(AdEvent::CampaignUpdated { campaign_id })
            }
        };
        let updated = StoredCampaign {
            intent: state.intent.clone(),
            remote_snapshot: state.last_known_remote,
            revision: stored.revision + 1,
            ..stored
        };
        if !self
            .campaigns
            .update(&mut scope, &updated, expected_revision)
            .await?
        {
            return Err(AdvertisingError::StaleRevision);
        }
        self.campaigns
            .append_change(
                &mut scope,
                &CampaignChange {
                    id: self.ids.next_campaign_change_id()?,
                    tenant_id,
                    campaign_id,
                    revision: updated.revision,
                    mutation_id: Some(mutation_id),
                    actor_id: Some(actor_id),
                    source: ChangeSource::Sanvi,
                    changed_fields,
                    before_state: before,
                    after_state: updated.intent.clone(),
                    attribution: None,
                    metadata: None,
                    created_at: Utc::now(),
                },
            )
            .await?;
        if let Some(event) = event {
            self.events
                .publish_scoped(&mut scope, tenant_id, &[event])
                .await?;
        }
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(ResolvedCampaignDrift {
            campaign: updated.intent,
            revision: updated.revision,
            replayed: false,
        })
    }
}
