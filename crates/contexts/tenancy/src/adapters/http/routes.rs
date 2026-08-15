//! Route registration: one function per mount point.

use actix_web::web;

use super::handlers::{
    activate_tenant, archive_tenant, get_context, get_settings, get_tenant, list_tenants,
    provision_tenant, public_tenant_context, put_settings, resume_tenant, slug_availability,
    suspend_tenant,
};

/// Platform-scoped tenant administration (also classified as a public
/// middleware prefix: no tenant resolution here).
pub fn platform_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1/platform/tenants")
            .route("", web::post().to(provision_tenant))
            .route("", web::get().to(list_tenants))
            .route("/{id}", web::get().to(get_tenant))
            .route("/{id}/activate", web::post().to(activate_tenant))
            .route("/{id}/suspend", web::post().to(suspend_tenant))
            .route("/{id}/resume", web::post().to(resume_tenant))
            .route("/{id}/archive", web::post().to(archive_tenant)),
    );
}

/// Tenant-scoped routes (require resolution; the middleware 404s unknown
/// hosts and 423s suspended tenants).
pub fn tenant_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1/tenant")
            .route("/context", web::get().to(get_context))
            .route("/settings", web::get().to(get_settings))
            .route("/settings", web::put().to(put_settings)),
    );
}

/// Public routes (no tenant required).
pub fn public_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1/public")
            .route("/slug-availability", web::get().to(slug_availability))
            .route("/tenant-context", web::get().to(public_tenant_context)),
    );
}
