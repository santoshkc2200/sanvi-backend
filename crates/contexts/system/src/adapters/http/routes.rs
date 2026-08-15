//! Route registration: one function per mount point.

use actix_web::web;

use super::handlers::{healthz, readyz, version};

/// Root-level health routes (`/healthz`, `/readyz`).
pub fn health_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/healthz").route(web::get().to(healthz)))
        .service(web::resource("/readyz").route(web::get().to(readyz)));
}

/// Context routes under `/api/v1/system`.
pub fn api_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/version").route(web::get().to(version)));
}
