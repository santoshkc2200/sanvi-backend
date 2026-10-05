//! Four-eyes approval use cases: list pending requests, approve, reject.
//! Approving executes the recorded action (currently: entitlement
//! overrides) through the `EntitlementAdmin` port, then marks the request
//! executed; both identities end up in the audit trail.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::application::storage;
use crate::domain::PlatformAdminError;
use crate::domain::approvals::{ApprovalId, ApprovalStatus, PendingApproval};
use crate::domain::ports::{Clock, EntitlementAdmin, IdGenerator, PendingApprovalRepository};

/// One approval request as exposed by the API.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ApprovalView {
    #[schema(value_type = String)]
    pub id: ApprovalId,
    #[schema(value_type = String)]
    pub requested_by: UserId,
    pub action_type: String,
    pub payload: serde_json::Value,
    pub status: ApprovalStatus,
    pub requested_at: DateTime<Utc>,
    #[schema(value_type = Option<String>)]
    pub decided_by: Option<UserId>,
    pub decided_at: Option<DateTime<Utc>>,
}

impl From<&PendingApproval> for ApprovalView {
    fn from(approval: &PendingApproval) -> Self {
        Self {
            id: approval.id(),
            requested_by: approval.requested_by(),
            action_type: approval.action_type().to_string(),
            payload: approval.payload().clone(),
            status: approval.status(),
            requested_at: approval.requested_at(),
            decided_by: approval.decided_by(),
            decided_at: approval.decided_at(),
        }
    }
}

pub struct ListPendingApprovals {
    tx_manager: TxManager,
    approvals: Arc<dyn PendingApprovalRepository>,
}

impl ListPendingApprovals {
    pub fn new(tx_manager: TxManager, approvals: Arc<dyn PendingApprovalRepository>) -> Self {
        Self {
            tx_manager,
            approvals,
        }
    }

    #[tracing::instrument(name = "platform_admin.list_pending_approvals", skip_all)]
    pub async fn execute(
        &self,
        limit: Option<u32>,
        after_id: Option<i64>,
    ) -> Result<Vec<ApprovalView>, PlatformAdminError> {
        let limit = sanvi_kernel::pagination::clamp_page_size(limit, 200, 200);
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_approvals", "platform-admin")
            .await
            .map_err(storage)?;
        let approvals = self
            .approvals
            .list_pending(&mut scope, limit, after_id)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(approvals.iter().map(ApprovalView::from).collect())
    }
}

pub struct DecideApprovalCommand {
    pub approval_id: ApprovalId,
    pub approve: bool,
}

pub struct ApproveRequest {
    inner: Arc<DecideApproval>,
}

pub struct RejectRequest {
    inner: Arc<DecideApproval>,
}

impl ApproveRequest {
    pub fn new(inner: Arc<DecideApproval>) -> Self {
        Self { inner }
    }
}

impl RejectRequest {
    pub fn new(inner: Arc<DecideApproval>) -> Self {
        Self { inner }
    }
}

pub struct DecideApproval {
    tx_manager: TxManager,
    approvals: Arc<dyn PendingApprovalRepository>,
    entitlements: Arc<dyn EntitlementAdmin>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl DecideApproval {
    pub fn new(
        tx_manager: TxManager,
        approvals: Arc<dyn PendingApprovalRepository>,
        entitlements: Arc<dyn EntitlementAdmin>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            approvals,
            entitlements,
            clock,
            audit,
        }
    }
}

impl ApproveRequest {
    #[tracing::instrument(name = "platform_admin.approve_request", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        cmd: DecideApprovalCommand,
    ) -> Result<ApprovalView, PlatformAdminError> {
        self.inner
            .execute_decision(actor, on_behalf_of, cmd, true)
            .await
    }
}

impl RejectRequest {
    #[tracing::instrument(name = "platform_admin.reject_request", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        cmd: DecideApprovalCommand,
    ) -> Result<ApprovalView, PlatformAdminError> {
        self.inner
            .execute_decision(actor, on_behalf_of, cmd, false)
            .await
    }
}

impl DecideApproval {
    async fn execute_decision(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        cmd: DecideApprovalCommand,
        approve: bool,
    ) -> Result<ApprovalView, PlatformAdminError> {
        let now = self.clock.now();
        let mut scope = self
            .tx_manager
            .begin_unscoped("decide_approval", "platform-admin")
            .await
            .map_err(storage)?;
        let mut approval = self
            .approvals
            .find_by_id(&mut scope, cmd.approval_id)
            .await?
            .ok_or(PlatformAdminError::ApprovalNotFound {
                id: cmd.approval_id,
            })?;
        approval.decide(actor, approve, now)?;

        // The decision itself (approved/rejected) is committed durably
        // *before* the cross-context executor runs: the two contexts do
        // not share a transaction (no cross-context database access), so
        // this ordering guarantees the approval is never left `Pending`
        // — and therefore never re-decidable — once execution has begun.
        self.approvals.save(&mut scope, &approval).await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: now,
                    actor_type: ActorType::PlatformAdmin,
                    actor_id: Some(actor),
                    on_behalf_of,
                    tenant_id: None,
                    action: if approve {
                        "platform_admin.approval_approved".to_string()
                    } else {
                        "platform_admin.approval_rejected".to_string()
                    },
                    resource_type: Some("approval".to_string()),
                    resource_id: Some(cmd.approval_id.to_string()),
                    before: Some(serde_json::json!({
                        "status": "pending",
                        "requested_by": approval.requested_by(),
                    })),
                    after: Some(serde_json::json!({
                        "status": approval.status().as_str(),
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(storage)?;
        scope.commit().await.map_err(storage)?;

        if !approve {
            return Ok(ApprovalView::from(&approval));
        }

        // Execute the approved action through the context that owns it.
        // If this fails, the approval durably stays `Approved` (not
        // `Executed`) rather than reverting to `Pending` or silently
        // reporting success — a recoverable, visible state instead of a
        // state that contradicts what actually happened.
        match approval.action_type() {
            "entitlement.grant" => {
                let payload = approval.payload();
                let tenant_id = payload
                    .get("tenant_id")
                    .and_then(|value| value.as_str())
                    .and_then(|value| value.parse::<TenantId>().ok())
                    .ok_or_else(|| {
                        PlatformAdminError::InvalidSearch(
                            "approval payload has no tenant_id".to_string(),
                        )
                    })?;
                let feature_key = payload
                    .get("feature_key")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        PlatformAdminError::InvalidSearch(
                            "approval payload has no feature_key".to_string(),
                        )
                    })?;
                let enabled = payload
                    .get("enabled")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                let limit = payload.get("limit").and_then(|value| value.as_i64());
                let expires_at = payload
                    .get("expires_at")
                    .and_then(|value| value.as_str())
                    .and_then(|value| {
                        chrono::DateTime::parse_from_rfc3339(value)
                            .ok()
                            .map(|dt| dt.with_timezone(&Utc))
                    });
                let reason = payload
                    .get("reason")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                self.entitlements
                    .grant_override_approved(
                        approval.requested_by(),
                        tenant_id,
                        feature_key.to_string(),
                        enabled,
                        limit,
                        expires_at,
                        reason,
                    )
                    .await?;
            }
            other => {
                return Err(PlatformAdminError::InvalidActionType(format!(
                    "no executor registered for approval action {other}"
                )));
            }
        }

        approval.mark_executed(now)?;
        let mut scope = self
            .tx_manager
            .begin_unscoped("decide_approval_executed", "platform-admin")
            .await
            .map_err(storage)?;
        self.approvals.save(&mut scope, &approval).await?;
        scope.commit().await.map_err(storage)?;
        Ok(ApprovalView::from(&approval))
    }
}

/// Constructs approvals (used by the `ApprovalGate` adapter the access
/// context consumes).
pub struct RecordApprovalRequest {
    approvals: Arc<dyn PendingApprovalRepository>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl RecordApprovalRequest {
    pub fn new(
        approvals: Arc<dyn PendingApprovalRepository>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            approvals,
            ids,
            clock,
        }
    }

    #[tracing::instrument(name = "platform_admin.record_approval_request", skip_all)]
    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        requested_by: UserId,
        action_type: &str,
        payload: serde_json::Value,
    ) -> Result<ApprovalId, PlatformAdminError> {
        let approval = PendingApproval::new(
            self.ids.next_approval_id()?,
            requested_by,
            action_type.to_string(),
            payload,
            self.clock.now(),
        )?;
        let id = approval.id();
        self.approvals.save(scope, &approval).await?;
        Ok(id)
    }
}
