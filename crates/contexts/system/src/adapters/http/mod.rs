//! HTTP driving adapter: handlers + route registration + OpenAPI.

mod handlers;
mod routes;

pub use handlers::{
    __path_healthz, __path_readyz, __path_version, SystemState, healthz, readyz, state_for_tests,
    version,
};
pub use routes::{api_routes, health_routes};

use utoipa::OpenApi;

/// OpenAPI document for this context; the API app nests it into the
/// full document served at `/openapi.json`.
#[derive(OpenApi)]
#[openapi(
    paths(handlers::healthz, handlers::readyz, handlers::version),
    components(schemas(
        crate::domain::BuildInfo,
        crate::domain::HealthState,
        crate::domain::DependencyStatus,
        crate::domain::Readiness,
        crate::application::LivenessResult,
        crate::application::ReadinessOutput,
    ))
)]
pub struct SystemApiDoc;
