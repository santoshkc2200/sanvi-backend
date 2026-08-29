//! Cross-tenant reconciliation sweep — the worker job behind TASK-012's
//! "nightly and on-demand pull of the remote tree per connection".
//!
//! This orchestrates the fetch; it never writes a drift observation itself.
//! Every applied result goes through the same [`ReconcileCampaign`] use
//! case an on-demand reconciliation call would use, so a scheduled pass and
//! an operator-triggered one can never disagree about what counts as drift.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Serialize;

use crate::{
    application::{PlatformRegistry, ReconcileCampaign},
    domain::{AdvertisingError, CampaignRepository, ConnectionHandle, ReconciliationResult},
};

const ACTOR: &str = "advertising-reconciler";

/// One sweep pass's outcome, for the job's log line and metrics.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ReconciliationSweepReport {
    /// Campaigns whose remote state was successfully fetched and applied.
    pub checked: u32,
    /// Of those, remote state changed without contradicting our intent.
    pub updated: u32,
    /// Of those, remote state contradicted our intent — drift was raised.
    pub drifted: u32,
    /// Adapter or storage failures; retried on the next pass.
    pub errors: u32,
}

pub struct ReconcileDueCampaigns {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    platforms: Arc<PlatformRegistry>,
    reconcile: Arc<ReconcileCampaign>,
}

impl ReconcileDueCampaigns {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        platforms: Arc<PlatformRegistry>,
        reconcile: Arc<ReconcileCampaign>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            platforms,
            reconcile,
        }
    }

    /// Pull up to `limit` published campaigns, oldest-checked-first, and
    /// reconcile each against its platform.
    pub async fn execute(&self, limit: i64) -> Result<ReconciliationSweepReport, AdvertisingError> {
        let mut report = ReconciliationSweepReport::default();

        let batch = {
            let mut scope = self
                .tx
                .begin_unscoped("advertising_reconciliation_sweep", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            let batch = self
                .campaigns
                .list_published_due_for_reconciliation(&mut scope, limit)
                .await?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            batch
        };

        for stored in batch {
            // The `external_id IS NOT NULL` filter guarantees this, but the
            // port takes a borrowed id either way.
            let Some(external_id) = stored.external_id.as_deref() else {
                continue;
            };
            let adapter = match self.platforms.adapter(&stored.platform) {
                Ok(adapter) => adapter,
                Err(_) => {
                    // Registry missing the platform (e.g. disabled in this
                    // environment): skip loudly rather than churn retries
                    // every pass.
                    tracing::warn!(
                        campaign_id = %stored.id,
                        platform = stored.platform.as_str(),
                        "advertising reconciliation skipped: adapter unavailable"
                    );
                    continue;
                }
            };
            let handle = ConnectionHandle {
                id: stored.connection_id.to_string(),
            };
            match adapter.get_campaign(&handle, external_id).await {
                Ok(Some(remote)) => {
                    match self
                        .reconcile
                        .execute(stored.tenant_id, stored.id, remote)
                        .await
                    {
                        Ok(ReconciliationResult::Unchanged) => {
                            self.mark_reconciled(stored.id).await?;
                            report.checked += 1;
                        }
                        Ok(ReconciliationResult::RemoteUpdated { .. }) => {
                            self.mark_reconciled(stored.id).await?;
                            report.checked += 1;
                            report.updated += 1;
                        }
                        Ok(ReconciliationResult::Drifted { .. }) => {
                            self.mark_reconciled(stored.id).await?;
                            report.checked += 1;
                            report.drifted += 1;
                        }
                        Err(error) => {
                            report.errors += 1;
                            tracing::error!(
                                campaign_id = %stored.id,
                                error = %error,
                                "advertising reconciliation apply failed"
                            );
                        }
                    }
                }
                Ok(None) => {
                    // Removed at the platform entirely. Surfacing this as
                    // drift needs a "removed" remote state distinct from
                    // "unchanged" — tracked as follow-up rather than guessed
                    // at here; a silent skip is safer than a false report.
                    self.mark_reconciled(stored.id).await?;
                    report.checked += 1;
                }
                Err(error) => {
                    report.errors += 1;
                    tracing::error!(
                        campaign_id = %stored.id,
                        error = %error,
                        "advertising reconciliation fetch failed"
                    );
                }
            }
        }

        Ok(report)
    }

    async fn mark_reconciled(
        &self,
        campaign_id: crate::domain::model::CampaignId,
    ) -> Result<(), AdvertisingError> {
        let mut scope = self
            .tx
            .begin_unscoped("advertising_reconciliation_sweep", ACTOR)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.campaigns
            .mark_reconciled(&mut scope, campaign_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))
    }
}
