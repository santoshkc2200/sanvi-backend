use thiserror::Error;

use crate::domain::values::{Email, ProviderId};
use crate::domain::{InvitationId, MembershipStatus, UserId};
use sanvi_kernel::{IdGenerationError, RoleId, TenantId};

/// Domain errors for the `identity` context.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IdentityError {
    #[error("invalid email address: {0}")]
    InvalidEmail(String),

    #[error("invalid provider id: {0}")]
    InvalidProvider(String),

    #[error("invalid locale: {0}")]
    InvalidLocale(String),

    #[error("invalid user status: {0}")]
    InvalidUserStatus(String),

    #[error("invalid membership status: {0}")]
    InvalidMembershipStatus(String),

    #[error("user {id} not found")]
    UserNotFound { id: UserId },

    #[error("no user with email {email}")]
    UserEmailNotFound { email: Email },

    #[error("user {id} is not active")]
    UserNotActive { id: UserId },

    #[error("illegal user status transition: {from} -> {to}")]
    IllegalUserTransition { from: String, to: String },

    #[error("user {user_id} is not a member of tenant {tenant_id}")]
    NotAMember {
        user_id: UserId,
        tenant_id: TenantId,
    },

    #[error("a member cannot remove their own membership")]
    CannotRemoveSelf,

    #[error(
        "membership status transition {from} -> {to} would remove the last holder of role {role}"
    )]
    LastRoleHolder {
        from: MembershipStatus,
        to: MembershipStatus,
        role: String,
    },

    #[error("provider account {provider}/{subject} is already linked to another user")]
    AccountLinkedToOtherUser {
        provider: ProviderId,
        subject: String,
    },

    #[error("account {provider} is already linked to this user")]
    AccountAlreadyLinked { provider: ProviderId },

    #[error("invitation {id} not found")]
    InvitationNotFound { id: InvitationId },

    #[error("invitation already accepted")]
    InvitationAlreadyAccepted,

    #[error("invitation expired")]
    InvitationExpired,

    #[error("invitation email does not match the signed-in user")]
    InvitationEmailMismatch,

    #[error("unknown role id {0}")]
    UnknownRole(RoleId),

    #[error("role id {0} is not assignable in this tenant")]
    RoleNotAssignable(RoleId),

    #[error("no roles specified")]
    NoRoles,

    #[error("linking intent not found, expired or already used")]
    LinkIntentUnavailable,

    #[error("linking intent email does not match the signed-in user")]
    LinkIntentEmailMismatch,

    #[error("session not found")]
    SessionNotFound,

    #[error("webhook payload is malformed: {0}")]
    MalformedWebhook(String),

    #[error("kratos is unreachable: {0}")]
    KratosUnreachable(String),

    #[error("kratos rejected the request: {0}")]
    KratosRejected(String),

    #[error("ID generation failure: {0}")]
    IdGeneration(String),

    #[error("storage failure")]
    Storage(String),
}

impl From<IdGenerationError> for IdentityError {
    fn from(error: IdGenerationError) -> Self {
        Self::IdGeneration(error.to_string())
    }
}

impl IdentityError {
    /// Map a parse failure of a status-like string into the domain error.
    pub fn invalid_user_status(value: &str) -> Self {
        Self::InvalidUserStatus(value.to_string())
    }

    pub fn invalid_membership_status(value: &str) -> Self {
        Self::InvalidMembershipStatus(value.to_string())
    }
}
