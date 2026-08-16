//! `access` — the context that answers "what may you do?".
//!
//! Domain: the central permission registry, the role catalog and the
//! `Authorizer` port. Application: the cached permission check every other
//! context calls, role seeding, and the Keto tuple rebuild/sync. Adapters:
//! the sqlx role repository, the Keto REST client (relation tuples), the
//! short-TTL decision cache, the event consumer that keeps Keto in sync
//! with identity/tenancy events, and the permissions endpoint.
//!
//! Keto (Ory) is the source of truth for "user X is a member of role Y";
//! the role → permission mapping lives here in Postgres. Nothing under
//! `src/domain/` imports infrastructure crates — enforced by
//! `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::AccessConfig;
pub use domain::{
    AccessError, Decision, Permission, PermissionDef, PermissionScope, Role, RoleId, RoleScope,
};

/// Wire access routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::access_routes;
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::catalog::ROUTES as PERMISSIONED_ROUTES;
}
