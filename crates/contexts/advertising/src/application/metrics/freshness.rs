//! Per-connection ingestion staleness (TASK-016, slice 10.7) — what the
//! dashboard's "still updating" / "sync failed" states render from, derived
//! straight from `AdvertisingConnection::last_metrics_ingested_at` rather
//! than a separate value that could drift from what actually happened.
//! Deliberately not `last_synced_at` — that field is the OAuth refresh
//! lifecycle's cursor, and a successful token refresh must never make a
//! connection whose metrics ingestion is failing (or has never run) report
//! as current.

use std::sync::Arc;

use chrono::Utc;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::{AdvertisingConnectionRepository, AdvertisingError, ConnectionFreshness};

pub struct GetFreshness {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    stalled_after_hours: f64,
}

impl GetFreshness {
    pub fn new(
        tx: TxManager,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        stalled_after_hours: f64,
    ) -> Self {
        Self {
            tx,
            connections,
            stalled_after_hours,
        }
    }

    #[tracing::instrument(name = "advertising.get_freshness", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<ConnectionFreshness>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let connections = self.connections.list(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let now = Utc::now();
        Ok(connections
            .into_iter()
            .map(|connection| {
                let last_ingested_at = connection.last_metrics_ingested_at();
                let lag_hours = last_ingested_at
                    .map(|synced_at| (now - synced_at).num_seconds() as f64 / 3600.0);
                // No ingestion yet is stalled by definition — never rendered
                // as current just because there is nothing to compare against.
                let stalled = lag_hours.is_none_or(|hours| hours > self.stalled_after_hours);
                ConnectionFreshness {
                    connection_id: connection.id(),
                    platform: connection.platform().clone(),
                    last_ingested_at,
                    lag_hours,
                    stalled,
                }
            })
            .collect())
    }
}
