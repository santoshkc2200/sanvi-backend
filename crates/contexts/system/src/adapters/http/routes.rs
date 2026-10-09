//! Route registration: one function per mount point.

use actix_web::web;

use super::handlers::{build, health, healthz, ready, readyz, version};

/// Root-level health routes (`/healthz`, `/readyz`).
pub fn health_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/healthz").route(web::get().to(healthz)))
        .service(web::resource("/readyz").route(web::get().to(readyz)));
}

/// Context routes under `/api/v1/system`.
pub fn api_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/version").route(web::get().to(version)))
        .service(web::resource("/build").route(web::get().to(build)))
        // TASK-025: the versioned probes an orchestrator or external
        // monitor calls when only the API surface is reachable. `/health`
        // is liveness only (never the database); `/ready` reports
        // dependency states and nothing else.
        .service(web::resource("/health").route(web::get().to(health)))
        .service(web::resource("/ready").route(web::get().to(ready)));
}
