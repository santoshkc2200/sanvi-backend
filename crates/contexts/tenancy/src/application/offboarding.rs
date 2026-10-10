//! Offboarding use cases (TASK-028, FR-1131).
//!
//! `RequestOffboarding` is the API's POST: it records the request,
//! idempotently by key, refusing tenants that are not archived yet (the
//! lifecycle gate: a live tenant must be archived first — offboarding
//! starts from the documented exit state). `GetOffboarding` is the GET:
//! status, artifact evidence, purge report. The export and purge
//! themselves are executor work (the CLI `offboard` command), which
//! advances this record through the verified-export gate.

use std::sync::Arc;

use chrono::Utc;
use sanvi_kernel::UserId;
use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::{
    Clock, OffboardingRepository, OffboardingRequest, OffboardingStatus, TenancyError, TenantId,
    TenantRepository,
};

use super::storage;

#[derive(Debug, Serialize, ToSchema)]
pub struct OffboardingView {
    /// Decimal Snowflake id (transport form).
    #[schema(value_type = String)]
    pub id: i64,
    #[schema(value_type = String)]
    pub tenant_id: i64,
    pub idempotency_key: String,
    pub status: String,
    /// Present once the export verified: the artifact path and
    /// whole-archive checksum the purge step required.
    pub export: Option<ExportEvidence>,
    /// The purge report: removed per context, lawfully retained with
    /// reason and retention end date.
    pub purge_report: Option<serde_json::Value>,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExportEvidence {
    pub path: String,
    pub checksum: String,
}

impl From<OffboardingRequest> for OffboardingView {
    fn from(request: OffboardingRequest) -> Self {
        Self {
            id: request.id,
            tenant_id: request.tenant_id.as_i64(),
            idempotency_key: request.idempotency_key,
            status: request.status.as_str().to_string(),
            export: request.export.map(|export| ExportEvidence {
                path: export.path,
                checksum: export.checksum,
            }),
            purge_report: request.purge_report,
            error: request.error,
            created_at: request.created_at,
            updated_at: request.updated_at,
            completed_at: request.completed_at,
        }
    }
}

pub struct RequestOffboarding {
    tx_manager: TxManager,
    tenants: Arc<dyn TenantRepository>,
    offboarding: Arc<dyn OffboardingRepository>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl RequestOffboarding {
    pub fn new(
        tx_manager: TxManager,
        tenants: Arc<dyn TenantRepository>,
        offboarding: Arc<dyn OffboardingRepository>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx_manager,
            tenants,
            offboarding,
            clock,
            ids,
        }
    }

    /// Record the request (idempotent by `(tenant, key)`). Returns the
    /// request — the existing one when the key repeats, so a retried
    /// POST is indistinguishable from the first.
    #[tracing::instrument(name = "tenancy.request_offboarding", skip_all)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        idempotency_key: String,
        requested_by: Option<UserId>,
    ) -> Result<OffboardingRequest, TenancyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("request_offboarding", "platform-admin")
            .await
            .map_err(storage)?;

        let tenant = self
            .tenants
            .find_by_id(&mut scope, tenant_id)
            .await?
            .ok_or(TenancyError::TenantNotFound {
                id: tenant_id.as_i64().to_string(),
            })?;

        // Idempotency first: a repeat POST answers the existing request
        // even if the tenant moved on — forking the record is worse than
        // answering the old one.
        if let Some(existing) = self
            .offboarding
            .find_by_tenant_and_key(&mut scope, tenant_id, &idempotency_key)
            .await?
        {
            scope.commit().await.map_err(storage)?;
            return Ok(existing);
        }

        // Lifecycle gate: offboarding starts from `archived` — the
        // documented exit state, after the operator has suspended and
        // archived the tenant through the normal lifecycle.
        if !matches!(
            tenant.status(),
            crate::domain::TenantStatus::Archived | crate::domain::TenantStatus::Suspended
        ) {
            return Err(TenancyError::Conflict(format!(
                "tenant {} is {}: offboarding starts from suspended or archived",
                tenant.slug().as_str(),
                tenant.status().as_str()
            )));
        }

        let now = self.clock.now();
        let request = OffboardingRequest {
            id: self.ids.next_tenant_id()?.as_i64(),
            tenant_id,
            idempotency_key,
            status: OffboardingStatus::Pending,
            requested_by: requested_by.map(|id| id.as_i64()),
            export: None,
            purge_report: None,
            error: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
        };
        let created = self.offboarding.create_or_get(&mut scope, &request).await?;
        scope.commit().await.map_err(storage)?;
        Ok(created)
    }
}

pub struct GetOffboarding {
    tx_manager: TxManager,
    offboarding: Arc<dyn OffboardingRepository>,
}

impl GetOffboarding {
    pub fn new(tx_manager: TxManager, offboarding: Arc<dyn OffboardingRepository>) -> Self {
        Self {
            tx_manager,
            offboarding,
        }
    }

    /// The tenant's latest offboarding request (404 when none).
    #[tracing::instrument(name = "tenancy.get_offboarding", skip_all)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<OffboardingRequest>, TenancyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("get_offboarding", "platform-admin")
            .await
            .map_err(storage)?;
        let found = self
            .offboarding
            .latest_for_tenant(&mut scope, tenant_id)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(found)
    }
}

/// Executor-side record keeping (used by the CLI `offboard` command):
/// the verified export, the terminal purge report, and failure notes.
pub struct AdvanceOffboarding {
    tx_manager: TxManager,
    offboarding: Arc<dyn OffboardingRepository>,
}

impl AdvanceOffboarding {
    pub fn new(tx_manager: TxManager, offboarding: Arc<dyn OffboardingRepository>) -> Self {
        Self {
            tx_manager,
            offboarding,
        }
    }

    /// The export gate: record the verified export evidence. Refuses a
    /// request that is not in `exporting`.
    #[tracing::instrument(name = "tenancy.mark_offboarding_exported", skip_all)]
    pub async fn mark_exported(
        &self,
        request_id: i64,
        path: String,
        checksum: String,
    ) -> Result<(), TenancyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("offboarding_exported", "cli:offboard")
            .await
            .map_err(storage)?;
        self.offboarding
            .mark_exported(
                &mut scope,
                request_id,
                &crate::domain::VerifiedExport { path, checksum },
            )
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(())
    }

    /// Advance the state machine. Enforces the transition table and the
    /// verified-export precondition for `purging` — the hard gate that
    /// replaces the two-person confirmation.
    #[tracing::instrument(name = "tenancy.advance_offboarding", skip_all)]
    pub async fn execute(
        &self,
        request_id: i64,
        next: OffboardingStatus,
        purge_report: Option<serde_json::Value>,
        error: Option<String>,
    ) -> Result<(), TenancyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("advance_offboarding", "cli:offboard")
            .await
            .map_err(storage)?;
        if next == OffboardingStatus::Purging {
            let current = self
                .offboarding
                .find_by_id(&mut scope, request_id)
                .await?
                .ok_or(TenancyError::TenantNotFound {
                    id: request_id.to_string(),
                })?;
            if !current.export_verified() {
                return Err(TenancyError::OffboardingExportMissing { request_id });
            }
        }
        self.offboarding
            .advance(&mut scope, request_id, next, purge_report, error)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(())
    }
}

/// Default clock (composition roots substitute for tests).
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        Utc::now()
    }
}
