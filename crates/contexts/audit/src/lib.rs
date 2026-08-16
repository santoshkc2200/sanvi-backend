//! `audit` — the business audit trail: append-only, hash-chained, tamper-evident.
//!
//! Domain: the [`AuditEntry`] value object (a hash link in a single global
//! chain), the pure chain verifier, and the [`AuditWriter`]/[`AuditReader`]
//! ports every other context uses to record who did what. Application: the
//! filterable audit query and the background chain verification. Adapters:
//! the sqlx writer/reader (appends are serialized with an advisory lock so
//! concurrent writers can never fork the chain), and the
//! `GET /api/v1/platform/audit` endpoint.
//!
//! `audit.entries` is append-only at two levels: the `sanvi_app` role has no
//! UPDATE/DELETE grants, and a BEFORE trigger raises on either. Nothing under
//! `src/domain/` imports infrastructure crates — enforced by
//! `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::AuditConfig;
pub use domain::{
    ActorType, AuditCursor, AuditEntry, AuditEntryData, AuditError, AuditFilters, AuditReader,
    AuditWriter, GENESIS_HASH,
};

/// Wire audit routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::audit_routes;
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::catalog::ROUTES as PERMISSIONED_ROUTES;
}
