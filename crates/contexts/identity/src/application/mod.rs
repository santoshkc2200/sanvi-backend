//! Application: use cases for the identity context.
//!
//! Transaction ownership follows the scoping rules: platform-scoped work
//! goes through `PlatformScope::begin_unscoped` (audited), tenant-scoped
//! work through `ScopedTx::begin_for` (RLS-filtered). Domain events are
//! written to the outbox in the same transaction as the state change.

pub mod delegation;
pub mod get_me;
pub mod handle_logout;
pub mod invitations;
pub mod link_challenge;
pub mod members;
pub mod sessions;
pub mod sync_identity;

pub use delegation::DelegationPolicy;

pub use get_me::{GetMe, MeView, MembershipSummary};
pub use handle_logout::HandleLogoutWebhook;
pub use invitations::{
    AcceptInvitation, AcceptInvitationCommand, InvitationStatus, InvitationView, InviteMember,
    InviteMemberCommand, ListInvitations, ResendInvitation, RevokeInvitation,
};
pub use link_challenge::{
    CompleteLinkChallenge, LinkChallengeResult, StartLinkChallenge, StartLinkChallengeCommand,
    StartLinkChallengeResponse,
};
pub use members::{
    GrantMembership, GrantMembershipCommand, ListMembers, MemberView, RemoveMember,
    UpdateMemberRoles, UpdateRolesCommand,
};
pub use sessions::{ListSessions, RevokeSession, SessionView};
pub use sync_identity::{Flow, SyncIdentityFromKratos, SyncIdentityInput};

/// Maps infrastructure failures into the domain vocabulary at the boundary.
pub(crate) fn storage(err: impl std::fmt::Display) -> crate::domain::IdentityError {
    crate::domain::IdentityError::Storage(err.to_string())
}
