//! Attributes Sanvi's own order-based revenue onto the metrics rollup
//! (TASK-016, slice 10.7). Runs immediately after `IngestConnectionMetrics`
//! for the same window, so the target `metrics_daily` rows already exist.
//!
//! Runs at **(tenant, platform)** grain, not per connection.
//! `ConversionEvent.upload_states` is keyed by platform only — it carries
//! no connection id (a TASK-014/015 constraint this slice inherits, not one
//! it introduces) — so a per-connection join would let a tenant's second
//! same-platform connection see and re-attribute the exact same event,
//! double-counting revenue across both connections' campaigns.
//!
//! Every call clears the whole requested window first — every platform
//! campaign's revenue reset to zero, every unattributed row for the window
//! deleted — then writes the freshly computed result, all in one
//! transaction. Without this, a retracted conversion or an attribution that
//! resolved from ambiguous to a single campaign would leave the old figure
//! behind instead of disappearing.
//!
//! Real per-event campaign attribution needs the platform's own click-id
//! decoding (blocked on live credentials, same as every other 10.2–10.6
//! gap). Until then: for a given day and currency, exactly one active
//! campaign on this platform (whose own ad account is in that currency)
//! attributes deterministically; anything else — zero or several candidate
//! campaigns, or a currency no candidate's account uses — is summed at
//! platform level, keyed by `(tenant, platform, date, currency)`, and never
//! distributed by a guess.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{NaiveDate, TimeZone, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::model::ConnectionId;
use crate::domain::{
    AdvertisingConnectionRepository, AdvertisingError, CampaignRepository, CampaignStatus,
    ConversionEventRepository, MetricsRepository, PlatformKey, ValueSource,
};

pub struct JoinCampaignRevenue {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    conversions: Arc<dyn ConversionEventRepository>,
    metrics_repo: Arc<dyn MetricsRepository>,
}

impl JoinCampaignRevenue {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        conversions: Arc<dyn ConversionEventRepository>,
        metrics_repo: Arc<dyn MetricsRepository>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            conversions,
            metrics_repo,
        }
    }

    #[tracing::instrument(name = "advertising.join_campaign_revenue", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        platform: &PlatformKey,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<(), AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let campaigns = self.campaigns.list_by_tenant(&mut scope, tenant_id).await?;
        let platform_campaigns: Vec<_> = campaigns
            .iter()
            .filter(|stored| &stored.platform == platform)
            .collect();
        let connections = self.connections.list(&mut scope, tenant_id).await?;
        let currency_by_connection: BTreeMap<ConnectionId, String> = connections
            .iter()
            .map(|connection| (connection.id(), connection.currency().to_string()))
            .collect();

        // Clear pass: every platform campaign's revenue reset, every
        // unattributed row for the window deleted — before anything new is
        // written, so a day this pass no longer finds revenue for actually
        // shows zero rather than retaining a stale figure.
        let mut cursor = from;
        loop {
            for campaign in &platform_campaigns {
                self.metrics_repo
                    .set_campaign_revenue(&mut scope, tenant_id, platform, campaign.id, cursor, 0)
                    .await?;
            }
            self.metrics_repo
                .clear_unattributed_revenue(&mut scope, tenant_id, platform, cursor)
                .await?;
            if cursor == to {
                break;
            }
            let Some(next) = cursor.succ_opt() else {
                break;
            };
            cursor = next;
        }

        let Some(window_start) = from.and_hms_opt(0, 0, 0) else {
            return Err(AdvertisingError::InvalidInput(
                "invalid revenue join window start".to_string(),
            ));
        };
        let Some(window_end) = to.and_hms_opt(23, 59, 59) else {
            return Err(AdvertisingError::InvalidInput(
                "invalid revenue join window end".to_string(),
            ));
        };
        let events = self
            .conversions
            .list_by_date_range(
                &mut scope,
                tenant_id,
                Utc.from_utc_datetime(&window_start),
                Utc.from_utc_datetime(&window_end),
            )
            .await?;

        // Revenue is bucketed by the event's own UTC calendar day and its
        // own currency. The platform's daily reporting is already bucketed
        // in the ad account's timezone; a purchase within a few hours of
        // midnight can therefore land in the adjacent day here versus in
        // `metrics_daily` — a documented gap, not a silent one, until this
        // reads the ad account's timezone rather than UTC.
        let mut revenue_by_day_currency: BTreeMap<(NaiveDate, String), i64> = BTreeMap::new();
        for event in &events {
            if event.order_ref.is_none() {
                continue;
            }
            if !matches!(event.value_source, Some(ValueSource::PaymentRecord)) {
                continue;
            }
            if !event.upload_states.contains_key(platform) {
                continue;
            }
            let Some(value) = &event.value else {
                continue;
            };
            let date = event.occurred_at.date_naive();
            *revenue_by_day_currency
                .entry((date, value.currency.clone()))
                .or_insert(0) += value.amount_minor;
        }

        for ((date, currency), revenue_minor) in revenue_by_day_currency {
            // Only a campaign whose own ad account already reports in this
            // currency is a valid attribution target — mixing currencies
            // into one row's ratio is never acceptable, and converting to
            // match would itself be a persisted converted value (forbidden).
            let candidates: Vec<_> = platform_campaigns
                .iter()
                .filter(|stored| stored.intent.status == CampaignStatus::Active)
                .filter(|stored| {
                    currency_by_connection
                        .get(&stored.connection_id)
                        .is_some_and(|account_currency| account_currency == &currency)
                })
                .map(|stored| stored.id)
                .collect();

            match candidates.as_slice() {
                [only_campaign] => {
                    self.metrics_repo
                        .set_campaign_revenue(
                            &mut scope,
                            tenant_id,
                            platform,
                            *only_campaign,
                            date,
                            revenue_minor,
                        )
                        .await?;
                }
                _ => {
                    self.metrics_repo
                        .set_unattributed_revenue(
                            &mut scope,
                            tenant_id,
                            platform,
                            date,
                            revenue_minor,
                            &currency,
                        )
                        .await?;
                }
            }
        }

        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(())
    }
}
