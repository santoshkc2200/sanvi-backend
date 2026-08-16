//! `access` — the context that answers "what may you do?" — and, since
//! phase 03, "what does this tenant have?".
//!
//! Domain: the central permission registry, the role catalog, the feature
//! catalog, entitlements with their resolution rules, and the
//! `Authorizer`/`FeatureGate` ports. Application: the cached permission
//! check every other context calls, role seeding and authoring, the Keto
//! tuple rebuild/sync, entitlement resolution/granting, and the feature
//! gate. Adapters: the sqlx repositories, the Keto REST client (relation
//! tuples), the decision and entitlement caches, the event consumers that
//! keep Keto and caches in sync, and the permissions/roles/features
//! endpoints.
//!
//! Keto (Ory) is the source of truth for "user X is a member of role Y";
//! the role → permission mapping lives here in Postgres. Entitlement
//! resolution composes `access.entitlements` overrides with
//! `access.features` defaults. Nothing under `src/domain/` imports
//! infrastructure crates — enforced by `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::AccessConfig;
pub use domain::{
    AccessError, Decision, Entitlement, EntitlementCache, EntitlementRepository, EntitlementSource,
    Feature, FeatureGate, FeatureKey, FeatureKind, FeatureRepository, Permission, PermissionDef,
    PermissionScope, QuotaUsageStore, ResolvedEntitlement, Role, RoleId, RoleScope, Visibility,
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
