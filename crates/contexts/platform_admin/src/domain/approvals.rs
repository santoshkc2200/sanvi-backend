//! Four-eyes approvals: a dangerous action is recorded as a pending
//! request; a *different* platform admin must decide before it executes.
//! Both identities are recorded.

use chrono::{DateTime, Utc};
use sanvi_kernel::Id;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::PlatformAdminError;

pub enum ApprovalTag {}
pub type ApprovalId = Id<ApprovalTag>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    /// The approved action has been executed.
    Executed,
}

impl ApprovalStatus {
    pub fn parse(raw: &str) -> Result<Self, PlatformAdminError> {
        match raw {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            "executed" => Ok(Self::Executed),
            other => Err(PlatformAdminError::InvalidSearch(format!(
                "unknown approval status: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Executed => "executed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    id: ApprovalId,
    requested_by: Uuid,
    action_type: String,
    payload: serde_json::Value,
    status: ApprovalStatus,
    requested_at: DateTime<Utc>,
    decided_by: Option<Uuid>,
    decided_at: Option<DateTime<Utc>>,
}

impl PendingApproval {
    pub fn new(
        id: ApprovalId,
        requested_by: Uuid,
        action_type: String,
        payload: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<Self, PlatformAdminError> {
        if action_type.trim().is_empty() {
            return Err(PlatformAdminError::InvalidActionType(action_type));
        }
        Ok(Self {
            id,
            requested_by,
            action_type,
            payload,
            status: ApprovalStatus::Pending,
            requested_at: now,
            decided_by: None,
            decided_at: None,
        })
    }

    /// Re-hydration constructor (no validation).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: ApprovalId,
        requested_by: Uuid,
        action_type: String,
        payload: serde_json::Value,
        status: ApprovalStatus,
        requested_at: DateTime<Utc>,
        decided_by: Option<Uuid>,
        decided_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id,
            requested_by,
            action_type,
            payload,
            status,
            requested_at,
            decided_by,
            decided_at,
        }
    }

    pub fn id(&self) -> ApprovalId {
        self.id
    }
    pub fn requested_by(&self) -> Uuid {
        self.requested_by
    }
    pub fn action_type(&self) -> &str {
        &self.action_type
    }
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }
    pub fn status(&self) -> ApprovalStatus {
        self.status
    }
    pub fn requested_at(&self) -> DateTime<Utc> {
        self.requested_at
    }
    pub fn decided_by(&self) -> Option<Uuid> {
        self.decided_by
    }
    pub fn decided_at(&self) -> Option<DateTime<Utc>> {
        self.decided_at
    }

    /// Decide a pending request. The requester may not decide their own
    /// request (the second pair of eyes).
    pub fn decide(
        &mut self,
        decided_by: Uuid,
        approve: bool,
        now: DateTime<Utc>,
    ) -> Result<(), PlatformAdminError> {
        if self.status != ApprovalStatus::Pending {
            return Err(PlatformAdminError::ApprovalNotPending {
                id: self.id.as_uuid(),
            });
        }
        if decided_by == self.requested_by {
            return Err(PlatformAdminError::SelfApproval);
        }
        self.status = if approve {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        };
        self.decided_by = Some(decided_by);
        self.decided_at = Some(now);
        Ok(())
    }

    /// Mark the approved action as executed (after the executor ran it).
    pub fn mark_executed(&mut self, now: DateTime<Utc>) -> Result<(), PlatformAdminError> {
        if self.status != ApprovalStatus::Approved {
            return Err(PlatformAdminError::ApprovalNotPending {
                id: self.id.as_uuid(),
            });
        }
        self.status = ApprovalStatus::Executed;
        self.decided_at = Some(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval() -> PendingApproval {
        PendingApproval::new(
            ApprovalId::new(),
            Uuid::now_v7(),
            "entitlement.grant".to_string(),
            serde_json::json!({ "feature": "domains.custom" }),
            Utc::now(),
        )
        .unwrap()
    }

    #[test]
    fn approval_requires_a_nonempty_action_type() {
        assert!(matches!(
            PendingApproval::new(
                ApprovalId::new(),
                Uuid::now_v7(),
                "  ".to_string(),
                serde_json::json!({}),
                Utc::now(),
            ),
            Err(PlatformAdminError::InvalidActionType(_))
        ));
    }

    #[test]
    fn a_second_pair_of_eyes_decides() {
        let mut approval = approval();
        let decider = Uuid::now_v7();
        assert_ne!(decider, approval.requested_by());
        approval.decide(decider, true, Utc::now()).unwrap();
        assert_eq!(approval.status(), ApprovalStatus::Approved);
        assert_eq!(approval.decided_by(), Some(decider));
    }

    #[test]
    fn the_requester_cannot_decide_their_own_request() {
        let mut approval = approval();
        let requester = approval.requested_by();
        assert!(matches!(
            approval.decide(requester, true, Utc::now()),
            Err(PlatformAdminError::SelfApproval)
        ));
    }

    #[test]
    fn decided_requests_cannot_be_decided_again() {
        let mut approval = approval();
        approval.decide(Uuid::now_v7(), false, Utc::now()).unwrap();
        assert!(matches!(
            approval.decide(Uuid::now_v7(), true, Utc::now()),
            Err(PlatformAdminError::ApprovalNotPending { .. })
        ));
    }

    #[test]
    fn only_approved_requests_can_execute() {
        let mut approval = approval();
        assert!(approval.mark_executed(Utc::now()).is_err());
        approval.decide(Uuid::now_v7(), true, Utc::now()).unwrap();
        approval.mark_executed(Utc::now()).unwrap();
        assert_eq!(approval.status(), ApprovalStatus::Executed);
    }
}
