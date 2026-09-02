//! Cross-tenant metrics ingestion sweep — the worker job behind TASK-016's
//! "hourly for today, daily backfill" requirement, collapsed into one
//! schedule (see the module doc on `RunMetricsIngestion::execute` for why).
//! Mirrors `application::reconciliation_sweep`'s shape: fetch the due batch
//! under an audited platform-wide transaction, then do the per-connection
//! work under a tenant-scoped one.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Duration, Utc};
use sanvi_persistence::TxManager;
use serde::Serialize;

use crate::adapters::AdvertisingMetrics;
use crate::domain::{AdvertisingConnection, AdvertisingConnectionRepository, AdvertisingError};

use super::{IngestConnectionMetrics, JoinCampaignRevenue};

const ACTOR: &str = "advertising-metrics-ingestion";

/// One sweep pass's outcome, for the job's log line and metrics.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct MetricsIngestionSweepReport {
    pub connections_checked: u32,
    pub rows_ingested: u64,
    pub errors: u32,
}

pub struct RunMetricsIngestion {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    ingest: Arc<IngestConnectionMetrics>,
    revenue_join: Arc<JoinCampaignRevenue>,
    metrics: Arc<AdvertisingMetrics>,
    backfill_days: i64,
}

impl RunMetricsIngestion {
    pub fn new(
        tx: TxManager,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        ingest: Arc<IngestConnectionMetrics>,
        revenue_join: Arc<JoinCampaignRevenue>,
        metrics: Arc<AdvertisingMetrics>,
        backfill_days: i64,
    ) -> Self {
        Self {
            tx,
            connections,
            ingest,
            revenue_join,
            metrics,
            backfill_days,
        }
    }

    /// Pull up to `limit` due connections and, for each, re-fetch and
    /// idempotently replace `[today - backfill_days + 1, today]` — a single
    /// schedule rather than separate hourly/daily timers: every tick
    /// re-covers the platform's restatement window, which is simpler and
    /// still correct (it just re-fetches a few more days than the strict
    /// minimum each time).
    pub async fn execute(
        &self,
        limit: i64,
    ) -> Result<MetricsIngestionSweepReport, AdvertisingError> {
        let mut report = MetricsIngestionSweepReport::default();
        let today = Utc::now().date_naive();
        let from = today - Duration::days((self.backfill_days.max(1)) - 1);

        let batch = {
            let mut scope = self
                .tx
                .begin_unscoped("advertising_metrics_ingestion_sweep", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            let batch = self
                .connections
                .list_due_for_metrics_ingestion(&mut scope, limit)
                .await?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            batch
        };

        let mut worst_lag_hours: f64 = 0.0;
        for connection in &batch {
            report.connections_checked += 1;
            let platform_key = connection.platform().clone();
            let platform = platform_key.as_str().to_string();
            let started = Instant::now();

            // Tracks what actually landed in storage this pass — used below
            // to compute lag from the post-sweep state rather than the
            // connection snapshot fetched before any of this ran, so a
            // successful pass reads as fresh and a failing one still
            // contributes its true (growing) staleness to the gauge instead
            // of being skipped.
            let mut persisted_ingested_at = connection.last_metrics_ingested_at();

            match self.ingest.execute(connection, from, today).await {
                Ok(count) => {
                    report.rows_ingested += count as u64;
                    self.metrics
                        .ads_sync_duration_seconds()
                        .with_label_values(&[&platform])
                        .observe(started.elapsed().as_secs_f64());

                    if let Err(error) = self
                        .revenue_join
                        .execute(connection.tenant_id(), &platform_key, from, today)
                        .await
                    {
                        report.errors += 1;
                        self.metrics
                            .ads_sync_failures_total()
                            .with_label_values(&[&platform, "revenue_join"])
                            .inc();
                        tracing::warn!(
                            connection_id = %connection.id(),
                            platform = %platform,
                            error = %error,
                            "advertising revenue join failed"
                        );
                    }

                    let now = Utc::now();
                    match self.mark_synced(connection, now).await {
                        Ok(()) => persisted_ingested_at = Some(now),
                        Err(error) => {
                            report.errors += 1;
                            tracing::warn!(
                                connection_id = %connection.id(),
                                error = %error,
                                "advertising metrics sync marker persist failed"
                            );
                        }
                    }
                }
                Err(error) => {
                    report.errors += 1;
                    self.metrics
                        .ads_sync_failures_total()
                        .with_label_values(&[&platform, error_code(&error)])
                        .inc();
                    tracing::warn!(
                        connection_id = %connection.id(),
                        platform = %platform,
                        error = %error,
                        "advertising metrics ingestion failed"
                    );
                }
            }

            // Never synced falls back to `connected_at()` — a real, always
            // present timestamp — rather than being skipped: a connection
            // that has failed every ingestion attempt since it was created
            // must still register as maximally stale, not leave the gauge
            // looking healthy at its default of 0.
            let reference = persisted_ingested_at.unwrap_or_else(|| connection.connected_at());
            let lag_hours = ((Utc::now() - reference).num_seconds() as f64 / 3600.0).max(0.0);
            worst_lag_hours = worst_lag_hours.max(lag_hours);
        }
        self.metrics
            .metrics_ingestion_lag_hours()
            .set(worst_lag_hours);

        Ok(report)
    }

    async fn mark_synced(
        &self,
        connection: &AdvertisingConnection,
        now: DateTime<Utc>,
    ) -> Result<(), AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(connection.tenant_id().as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let mut updated = connection.clone();
        updated.record_metrics_sync(now);
        self.connections.update(&mut scope, &updated).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(())
    }
}

fn error_code(error: &AdvertisingError) -> &'static str {
    match error {
        AdvertisingError::InvalidInput(_) => "invalid_input",
        AdvertisingError::PlatformUnavailable(_) => "platform_unavailable",
        AdvertisingError::Storage(_) => "storage",
        AdvertisingError::StaleRevision => "stale_revision",
        AdvertisingError::CampaignNotFound => "campaign_not_found",
        AdvertisingError::Validation(_) => "validation",
        AdvertisingError::ConnectionNotActive => "connection_not_active",
        AdvertisingError::ConnectionNotFound => "connection_not_found",
        AdvertisingError::ConnectionTokenMissing => "connection_token_missing",
        AdvertisingError::CampaignNotPublished => "campaign_not_published",
        AdvertisingError::AdGroupNotFound => "ad_group_not_found",
        AdvertisingError::AdNotFound => "ad_not_found",
        AdvertisingError::CreativeNotFound => "creative_not_found",
        AdvertisingError::ConversionNotFound => "conversion_not_found",
        AdvertisingError::AudienceNotFound => "audience_not_found",
        AdvertisingError::BudgetCapNotFound => "budget_cap_not_found",
        AdvertisingError::BudgetAlertNotFound => "budget_alert_not_found",
        AdvertisingError::CapBelowCurrentSpend => "cap_below_current_spend",
        AdvertisingError::EncryptionUnconfigured => "encryption_unconfigured",
        AdvertisingError::EncryptionFailure(_) => "encryption_failure",
        AdvertisingError::IdGeneration(_) => "id_generation",
    }
}
