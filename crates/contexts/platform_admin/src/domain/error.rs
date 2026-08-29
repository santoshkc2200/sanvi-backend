use crate::domain::{ApprovalId, ImpersonationGrantId};
use sanvi_kernel::{IdGenerationError, TenantId, UserId};
use thiserror::Error;

/// Domain errors for the `platform_admin` context.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PlatformAdminError {
    #[error("impersonation reason is required")]
    MissingReason,

    #[error("impersonation duration exceeds the maximum of {max_minutes} minutes")]
    DurationTooLong { max_minutes: u32 },

    #[error("impersonation duration must be positive")]
    InvalidDuration,

    #[error("impersonation grant {id} not found")]
    GrantNotFound { id: ImpersonationGrantId },

    #[error("impersonation grant {id} has been revoked")]
    GrantRevoked { id: ImpersonationGrantId },

    #[error("impersonation grant {id} has expired")]
    GrantExpired { id: ImpersonationGrantId },

    #[error("impersonation target {user_id} is not an active member of tenant {tenant_id}")]
    TargetNotMember {
        user_id: UserId,
        tenant_id: TenantId,
    },

    #[error("impersonation is refused: tenant {tenant_id} has an active privacy lock")]
    PrivacyLocked { tenant_id: TenantId },

    #[error("tenant {id} not found")]
    TenantNotFound { id: TenantId },

    #[error("approval {id} not found")]
    ApprovalNotFound { id: ApprovalId },

    #[error("approval {id} is not pending")]
    ApprovalNotPending { id: ApprovalId },

    #[error("the requester cannot decide their own approval request")]
    SelfApproval,

    #[error("invalid approval action type: {0}")]
    InvalidActionType(String),

    #[error("invalid entitlement request: {0}")]
    InvalidEntitlement(String),

    #[error("feature {0} not found")]
    FeatureNotFound(String),

    #[error("entitlement change rejected: {0}")]
    EntitlementConflict(String),

    #[error("invalid search: {0}")]
    InvalidSearch(String),

    #[error("invalid application-fee configuration: {0}")]
    InvalidFeeConfig(String),

    #[error("storage failure")]
    Storage(String),

    #[error("ID generation failure: {0}")]
    IdGeneration(String),
}

impl From<IdGenerationError> for PlatformAdminError {
    fn from(error: IdGenerationError) -> Self {
        Self::IdGeneration(error.to_string())
    }
}
