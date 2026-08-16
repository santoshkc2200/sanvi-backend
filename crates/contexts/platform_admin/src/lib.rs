//! `platform_admin` — the operator's control plane: tenant administration
//! read models, impersonation with guardrails, and four-eyes approvals.
//!
//! Domain: the [`ImpersonationGrant`] aggregate (time-boxed, read-only by
//! default, revocable), [`PendingApproval`] (a second admin must decide),
//! and the operator tenant view. Application: create/list/revoke
//! impersonation, tenant search, approval decisions. Adapters: sqlx
//! repositories, the `sanvi_http::ImpersonationVerifier` implementation the
//! middleware consumes, the `access` context's `ApprovalGate` port, and the
//! HTTP endpoints under `/api/v1/platform/...`.
//!
//! Nothing under `src/domain/` imports infrastructure crates — enforced by
//! `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::PlatformAdminConfig;
pub use domain::{
    ImpersonationGrant, ImpersonationGrantId, ImpersonationMode, PendingApproval,
    PlatformAdminError,
};

/// Wire platform-admin routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::platform_admin_routes;
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::catalog::ROUTES as PERMISSIONED_ROUTES;
}
