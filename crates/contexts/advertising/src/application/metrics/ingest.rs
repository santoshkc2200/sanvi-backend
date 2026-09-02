//! Per-connection metrics ingestion (TASK-016, slice 10.7). The worker sweep
//! calls this once per due connection; `JoinCampaignRevenue` runs
//! immediately after, over the same window, in `revenue_join.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::NaiveDate;
use sanvi_persistence::TxManager;

use crate::application::PlatformRegistry;
use crate::domain::{
    AdvertisingConnection, AdvertisingError, CampaignRepository, ConnectionHandle, MetricQuery,
    MetricsDailyRow, MetricsRepository,
};

pub struct IngestConnectionMetrics {
    tx: TxManager,
    platforms: Arc<PlatformRegistry>,
    campaigns: Arc<dyn CampaignRepository>,
    metrics_repo: Arc<dyn MetricsRepository>,
}

impl IngestConnectionMetrics {
    pub fn new(
        tx: TxManager,
        platforms: Arc<PlatformRegistry>,
        campaigns: Arc<dyn CampaignRepository>,
        metrics_repo: Arc<dyn MetricsRepository>,
    ) -> Self {
        Self {
            tx,
            platforms,
            campaigns,
            metrics_repo,
        }
    }

    /// Fetch `[from, to]` (inclusive) from the connection's platform and
    /// replace this connection's rows for that window in the rollup —
    /// the whole window, not just the days the fetch mentions: a
    /// restatement that drops a previously reported campaign/day (a
    /// campaign paused, deleted, or simply reporting nothing for a day)
    /// must make that row disappear rather than leave stale non-zero
    /// values behind. A campaign the platform reports that Sanvi has not
    /// recorded (not yet synced, or synced under a different connection)
    /// is skipped rather than guessed at — logged so a persistent
    /// mismatch is visible operationally.
    pub async fn execute(
        &self,
        connection: &AdvertisingConnection,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<usize, AdvertisingError> {
        let adapter = self.platforms.adapter(connection.platform())?;
        let handle = ConnectionHandle {
            id: connection.id().to_string(),
            tenant_id: connection.tenant_id(),
        };
        let query = MetricQuery {
            from: from.to_string(),
            to: to.to_string(),
        };
        let points = adapter.fetch_metrics(&handle, &query).await?;

        let mut scope = self
            .tx
            .begin_for(connection.tenant_id().as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let campaigns = self
            .campaigns
            .list_by_tenant(&mut scope, connection.tenant_id())
            .await?;
        let by_external: BTreeMap<&str, _> = campaigns
            .iter()
            .filter(|stored| stored.connection_id == connection.id())
            .filter_map(|stored| stored.external_id.as_deref().map(|ext| (ext, stored.id)))
            .collect();

        let mut rows = Vec::with_capacity(points.len());
        for point in &points {
            let Some(&campaign_id) = by_external.get(point.campaign_id.as_str()) else {
                tracing::warn!(
                    connection_id = %connection.id(),
                    platform = connection.platform().as_str(),
                    external_campaign_id = %point.campaign_id,
                    "advertising metrics ingestion skipped: campaign not recorded for this connection"
                );
                continue;
            };
            let Ok(date) = NaiveDate::parse_from_str(&point.date, "%Y-%m-%d") else {
                tracing::warn!(
                    connection_id = %connection.id(),
                    date = %point.date,
                    "advertising metrics ingestion skipped: unparseable date from adapter"
                );
                continue;
            };
            rows.push(MetricsDailyRow {
                tenant_id: connection.tenant_id(),
                platform: connection.platform().clone(),
                connection_id: connection.id(),
                campaign_id,
                date,
                impressions: point.impressions,
                clicks: point.clicks,
                spend: point.spend.clone(),
                conversions: point.conversions,
                conversion_value: point.conversion_value.clone(),
            });
        }

        let ingested = rows.len();
        // Clear the whole requested window for this connection first, in
        // the same transaction, so a day/campaign the fetch no longer
        // mentions is deleted rather than left stale.
        self.metrics_repo
            .replace_window(
                &mut scope,
                connection.tenant_id(),
                connection.platform(),
                connection.id(),
                from,
                to,
                &rows,
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(ingested)
    }
}
