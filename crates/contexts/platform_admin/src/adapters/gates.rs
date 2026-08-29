//! The `access` context's `ApprovalGate` port implementation: records a
//! four-eyes pending approval when the policy requires one. Lives here so
//! the access context never depends on `platform_admin` (the composition
//! root wires this adapter into `GrantEntitlement`).

use async_trait::async_trait;
use sanvi_access::domain::AccessError;
use sanvi_access::domain::ports::{ApprovalGate, ApprovalOutcome};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::PlatformScope;

use crate::application::approvals::RecordApprovalRequest;

/// A gate that always requires approval (used for actions marked
/// `requires_approval` unconditionally) — the request is recorded and the
/// caller receives the approval id.
#[derive(Clone)]
pub struct ApprovalGateAdapter {
    record: std::sync::Arc<RecordApprovalRequest>,
}

impl ApprovalGateAdapter {
    pub fn new(record: std::sync::Arc<RecordApprovalRequest>) -> Self {
        Self { record }
    }
}

#[async_trait]
impl ApprovalGate for ApprovalGateAdapter {
    async fn request_if_required(
        &self,
        scope: &mut PlatformScope<'_>,
        requested_by: UserId,
        action_type: &str,
        payload: serde_json::Value,
    ) -> Result<ApprovalOutcome, AccessError> {
        let approval_id = self
            .record
            .execute(scope, requested_by, action_type, payload)
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(ApprovalOutcome::Required {
            approval_id: approval_id.as_snowflake(),
        })
    }
}

/// A gate that never requires approval (four-eyes disabled: everything
/// executes immediately).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopApprovalGate;

#[async_trait]
impl ApprovalGate for NoopApprovalGate {
    async fn request_if_required(
        &self,
        _scope: &mut PlatformScope<'_>,
        _requested_by: UserId,
        _action_type: &str,
        _payload: serde_json::Value,
    ) -> Result<ApprovalOutcome, AccessError> {
        Ok(ApprovalOutcome::NotRequired)
    }
}

/// A privacy lock that never locks (phase 05 wires the jurisdiction-driven
/// implementation).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopPrivacyLock;

#[async_trait]
impl crate::domain::ports::PrivacyLock for NoopPrivacyLock {
    async fn is_locked(
        &self,
        _tenant_id: TenantId,
    ) -> Result<bool, crate::domain::PlatformAdminError> {
        Ok(false)
    }
}
