//! The connection-status reconciliation job (sub-phase 09.3).
//!
//! Webhooks are the fast path; this job is the safety net that makes a
//! lost webhook a delay instead of a bug. Periodically it re-syncs every
//! live connection not seen recently through the provider's authoritative
//! state, applying snapshots through the **same** lifecycle path the
//! webhook dispatcher uses — so a sandbox account reaches `Active` with
//! webhooks disabled entirely (the lost-webhook drill), and live events
//! and repairs can never disagree about semantics.

use std::sync::Arc;

use chrono::{Duration, Utc};
use serde::Serialize;

use crate::adapters::PaymentsMetrics;
use crate::application::ProviderRegistry;
use crate::application::lifecycle::{AppliedTransition, apply_account_snapshot};
use crate::domain::error::PaymentsError;
use crate::domain::events::EventPublisher;
use crate::domain::ports::ConnectionLifecycleStore;
use sanvi_persistence::TxManager;

const ACTOR: &str = "payments-reconciler";

/// One reconciliation pass's outcome.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct SyncReport {
    /// Connections examined against the provider.
    pub checked: u32,
    /// Connections whose stored truth changed (any status move or
    /// projection refresh).
    pub updated: u32,
    /// Provider/adapter failures — retried on the next pass.
    pub errors: u32,
}

pub struct SyncConnections {
    tx_manager: TxManager,
    connections: Arc<dyn ConnectionLifecycleStore>,
    registry: Arc<ProviderRegistry>,
    events: Arc<dyn EventPublisher>,
    /// Sync connections whose last observation is older than this.
    stale_after: Duration,
    metrics: Option<PaymentsMetrics>,
}

impl SyncConnections {
    pub fn new(
        tx_manager: TxManager,
        connections: Arc<dyn ConnectionLifecycleStore>,
        registry: Arc<ProviderRegistry>,
        events: Arc<dyn EventPublisher>,
        stale_after: Duration,
    ) -> Self {
        Self {
            tx_manager,
            connections,
            registry,
            events,
            stale_after,
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: PaymentsMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Re-sync up to `limit` stale connections, oldest observation first.
    pub async fn execute(&self, limit: i64) -> Result<SyncReport, PaymentsError> {
        let stale_before = Utc::now() - self.stale_after;
        let mut report = SyncReport::default();

        let batch = {
            let mut scope = self
                .tx_manager
                .begin_unscoped("list_connections_due_for_sync", ACTOR)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            let batch = self
                .connections
                .list_due_for_sync(&mut scope, stale_before, limit)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            batch
        };

        for connection in batch {
            let Some(adapter) = self.registry.get(connection.provider) else {
                // Registry missing the provider (e.g. no Connect key in
                // this environment): not a per-connection error — skip
                // loudly rather than churn attempts.
                tracing::warn!(
                    connection_id = %connection.id,
                    provider = %connection.provider.as_str(),
                    "reconciliation skipped: provider adapter unavailable"
                );
                continue;
            };
            match adapter.sync(&connection.external_account_id).await {
                Ok(snapshot) => match self.apply(&connection, &snapshot).await {
                    Ok(AppliedTransition::Changed) => {
                        report.checked += 1;
                        report.updated += 1;
                    }
                    // Timestamp-only touches are healthy passes, not repairs.
                    Ok(AppliedTransition::Touched | AppliedTransition::Unchanged) => {
                        report.checked += 1;
                    }
                    Err(err) => {
                        report.errors += 1;
                        tracing::error!(
                            connection_id = %connection.id,
                            error = %err,
                            "connection reconciliation apply failed",
                        );
                    }
                },
                Err(err) => {
                    report.errors += 1;
                    // Adapter-level failures log their own detail; this is
                    // the alertable summary line.
                    error_summary(&connection, &err);
                }
            }
        }

        if let Some(metrics) = &self.metrics {
            metrics.reconciliation_checked.inc_by(report.checked as u64);
            metrics.reconciliation_updated.inc_by(report.updated as u64);
        }
        Ok(report)
    }

    /// Persist one snapshot through the shared lifecycle path in its own
    /// transaction — identical semantics to the webhook dispatcher.
    async fn apply(
        &self,
        connection: &crate::domain::ProviderConnection,
        snapshot: &crate::domain::ports::ProviderAccountStatus,
    ) -> Result<AppliedTransition, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("apply_connection_sync", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let outcome = apply_account_snapshot(
            self.connections.as_ref(),
            self.events.as_ref(),
            &mut scope,
            connection,
            snapshot,
            Utc::now(),
        )
        .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(outcome)
    }
}

fn error_summary(connection: &crate::domain::ProviderConnection, err: &PaymentsError) {
    tracing::error!(
        connection_id = %connection.id,
        account = %connection.external_account_id,
        error = %err,
        "connection reconciliation sync failed",
    );
}
