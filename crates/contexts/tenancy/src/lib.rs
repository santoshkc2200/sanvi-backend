//! `tenancy` — the context that answers "which tenant is this?".
//!
//! Domain: the tenant aggregate (lifecycle state machine), value objects
//! (`TenantSlug`, `DataRegion`, `Locale`), domain events and ports.
//! Application: provisioning, lifecycle transitions, settings and context
//! use cases. Adapters: sqlx repositories (constructible only from scoped
//! transactions), host/slug resolution with caching, HTTP handlers.
//!
//! Nothing under `src/domain/` imports infrastructure crates — enforced by
//! `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::TenancyConfig;
pub use domain::{
    DataRegion, Locale, SuspensionReason, Tenant, TenantId, TenantName, TenantSearch,
    TenantSearchCursor, TenantSlug, TenantStatus, TenantTag,
};

/// Wire tenant-scoped routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::{
        platform_routes, public_routes, tenant_routes, tenant_scope_routes,
    };
}
