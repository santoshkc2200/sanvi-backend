//! `system` — the reference context.
//!
//! A deliberately trivial bounded context proving the full hexagonal slice:
//! domain (build info, health vocabulary, probe port) → application (use
//! cases) → adapters (HTTP handlers as the driving adapter, Postgres/Redis
//! probes as driven adapters). Every later context copies this shape.
//!
//! The layering rule is visible here: `domain` depends only on `sanvi-kernel`
//! and `serde`; `sqlx` and `actix` appear only under `adapters`.

pub mod adapters;
pub mod application;
pub mod domain;

pub use adapters::http::{SystemApiDoc, SystemState, state_for_tests};
pub use domain::{
    BuildDetails, BuildInfo, DependencyState, DependencyStatus, HealthState, Readiness,
    ReadinessStates,
};

/// Wire routes into an actix `ServiceConfig` at the given scope.
pub mod routes {
    pub use crate::adapters::http::{api_routes, health_routes};
}
