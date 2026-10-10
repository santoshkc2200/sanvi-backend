//! Erasure propagation (TASK-029, FR-1134): the destinations an erasure
//! must reach *after* the provider registry has done its work — the
//! archives, the backup window, the caches, the search indexes and the
//! ad-platform suppression lists. Every destination is either purged or
//! documented as lawfully retained with an end date, and the outcome is
//! written to `privacy.erasure_propagations` — the ledger the compliance
//! drills and the evidence packs read back.
//!
//! The job is idempotent and resumable: it picks up erasure requests that
//! completed since the last pass and have no ledger rows yet, so a crash
//! between the saga and the propagation cannot leave a destination
//! silently unreached.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;

use sanvi_persistence::TxManager;

use crate::domain::{ErasureDestination, PrivacyError, SubjectRef};

/// One destination's ledger row, as the drills and packs read it back.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PropagationRecord {
    pub request_id: i64,
    pub subject_key: String,
    pub destination: String,
    pub status: String,
    pub purged: i64,
    pub retained: serde_json::Value,
    pub propagated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct PropagateErasureReport {
    pub requests_propagated: usize,
    pub destinations_recorded: usize,
    pub failed: usize,
}

/// A completed erasure request that still owes propagation rows.
struct PendingErasure {
    request_id: i64,
    subject_ref: SubjectRef,
    tenant_id: Option<sanvi_kernel::TenantId>,
}

pub struct PropagateErasure {
    tx_manager: TxManager,
    destinations: Vec<Arc<dyn ErasureDestination>>,
}

impl PropagateErasure {
    pub fn new(tx_manager: TxManager, destinations: Vec<Arc<dyn ErasureDestination>>) -> Self {
        Self {
            tx_manager,
            destinations,
        }
    }

    #[tracing::instrument(name = "privacy.propagate_erasure", skip_all)]
    pub async fn execute(&self, limit: u32) -> Result<PropagateErasureReport, PrivacyError> {
        let mut report = PropagateErasureReport::default();
        let pending = self.pending(limit).await?;
        for request in pending {
            match self.propagate_one(&request).await {
                Ok(count) => {
                    report.destinations_recorded += count;
                    report.requests_propagated += 1;
                }
                Err(err) => {
                    report.failed += 1;
                    tracing::error!(
                        error = %err,
                        request_id = request.request_id,
                        "erasure propagation failed",
                    );
                }
            }
        }
        Ok(report)
    }

    /// Completed erasure requests that still owe at least one destination.
    async fn pending(&self, limit: u32) -> Result<Vec<PendingErasure>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_erasure_propagation_pending", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rows: Vec<(i64, serde_json::Value)> = sqlx::query_as(
            "SELECT r.id, r.subject_ref
             FROM privacy.data_subject_requests r
             WHERE r.kind = 'erasure'
               AND r.status = 'completed'
               AND NOT EXISTS (
                   SELECT 1 FROM privacy.erasure_propagations p
                   WHERE p.request_id = r.id
               )
             ORDER BY r.id
             LIMIT $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(rows
            .into_iter()
            .filter_map(|(id, subject_ref)| {
                let subject_ref: SubjectRef = serde_json::from_value(subject_ref).ok()?;
                Some(PendingErasure {
                    request_id: id,
                    tenant_id: subject_ref.tenant_id,
                    subject_ref,
                })
            })
            .collect())
    }

    async fn propagate_one(&self, request: &PendingErasure) -> Result<usize, PrivacyError> {
        let mut recorded = 0;
        for destination in &self.destinations {
            let now = Utc::now();
            let (status, purged, retained, detail) = match destination
                .propagate_erasure(&request.subject_ref, request.tenant_id)
                .await
            {
                Ok(outcome) => {
                    let retained = serde_json::to_value(&outcome.retained).unwrap_or_default();
                    (
                        outcome.status().to_string(),
                        outcome.purged as i64,
                        retained,
                        serde_json::json!({ "description": destination.description() }),
                    )
                }
                Err(err) => (
                    // A failed destination is ledgered as failed — a
                    // destination with no row would look like a
                    // destination that was never owed.
                    "failed".to_string(),
                    0,
                    serde_json::json!([]),
                    serde_json::json!({
                        "description": destination.description(),
                        "error": err.to_string(),
                    }),
                ),
            };
            self.persist_one(
                request,
                destination.name(),
                &status,
                purged,
                retained,
                detail,
                now,
            )
            .await?;
            recorded += 1;
        }
        Ok(recorded)
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_one(
        &self,
        request: &PendingErasure,
        destination: &str,
        status: &str,
        purged: i64,
        retained: serde_json::Value,
        detail: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<(), PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_erasure_propagation_record", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        sqlx::query(
            "INSERT INTO privacy.erasure_propagations
                 (request_id, subject_key, destination, purged, retained, status, detail, propagated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (request_id, destination) DO UPDATE SET
                 purged = EXCLUDED.purged,
                 retained = EXCLUDED.retained,
                 status = EXCLUDED.status,
                 detail = EXCLUDED.detail,
                 propagated_at = EXCLUDED.propagated_at",
        )
        .bind(request.request_id)
        .bind(request.subject_ref.canonical_key())
        .bind(destination)
        .bind(purged)
        .bind(retained)
        .bind(status)
        .bind(detail)
        .bind(now)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}
