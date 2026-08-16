//! Application: use cases for the platform_admin context.

pub mod approvals;
pub mod impersonation;
pub mod tenant_views;

pub use approvals::{ApproveRequest, DecideApprovalCommand, ListPendingApprovals, RejectRequest};
pub use impersonation::{
    CreateImpersonation, CreateImpersonationCommand, ImpersonationGrantView, ListImpersonations,
    RevokeImpersonation,
};
pub use tenant_views::{GetTenantAdminView, SearchTenantAdminViews};

/// Maps infrastructure failures into the domain vocabulary at the boundary.
pub(crate) fn storage(err: impl std::fmt::Display) -> crate::domain::PlatformAdminError {
    crate::domain::PlatformAdminError::Storage(err.to_string())
}
