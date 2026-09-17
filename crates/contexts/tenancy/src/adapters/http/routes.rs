//! Route registration: one function per mount point.
//!
//! Permission declarations ride on each resource's `app_data` (read back by
//! the `RequiredPermission` extractor) and are mirrored in
//! [`PERMISSIONED_ROUTES`] for the "no route without a permission" CI test.

use actix_web::http::Method;
use actix_web::web;
use sanvi_http::{ResourceSpec, require_permission};

use super::handlers::{
    activate_tenant, archive_tenant, get_context, get_settings, get_tenant, list_tenants,
    provision_tenant, public_tenant_context, put_settings, resume_tenant, slug_availability,
    suspend_tenant,
};

/// Platform-scoped tenant administration (classified as a public middleware
/// prefix — no tenant resolution — but permission-gated since phase 02).
pub fn platform_routes(cfg: &mut web::ServiceConfig) {
    let manage = require_permission!("platform.tenant.manage", platform);
    cfg.service(
        web::scope("/api/v1/platform/tenants")
            .app_data(manage)
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
/// hosts and 423s suspended tenants). Mounted *inside* the shared
/// `/api/v1/tenant` scope together with the identity context's member and
/// invitation routes — two scopes with the same path would shadow each
/// other in actix's router.
pub fn tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let settings_map = sanvi_http::PermissionMap::new(vec![
        (
            Method::GET,
            require_permission!("tenancy.settings.read", tenant),
        ),
        (
            Method::PUT,
            require_permission!("tenancy.settings.update", tenant),
        ),
    ]);
    cfg.route("/context", web::get().to(get_context)).service(
        web::resource("/settings")
            .app_data(settings_map)
            .route(web::get().to(get_settings))
            .route(web::put().to(put_settings)),
    );
}

/// The `/api/v1/tenant` scope with the tenancy context's own routes.
pub fn tenant_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::scope("/api/v1/tenant").configure(tenant_scope_routes));
}

/// Public routes (no tenant required).
///
/// Deliberately *not* a `web::scope("/api/v1/public")`: several contexts
/// contribute public routes under the same prefix, and actix commits to the
/// first matching scope without backtracking, so every later scope (and any
/// resource registered after it) 404s — invisible while each context is
/// tested against its own router alone. Full-path resources are order-safe.
pub fn public_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/v1/public/slug-availability").route(web::get().to(slug_availability)),
    )
    .service(
        web::resource("/api/v1/public/tenant-context").route(web::get().to(public_tenant_context)),
    );
}

/// The non-public tenancy routes with their permission requirements —
/// enumerated by the permission catalog CI test. `/api/v1/tenant/context`
/// is the explicitly-declared public exemption (bootstrap endpoint).
pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[
    (
        "/api/v1/platform/tenants",
        Method::POST,
        "platform.tenant.manage",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/tenants",
        Method::GET,
        "platform.tenant.manage",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/tenants/{id}",
        Method::GET,
        "platform.tenant.manage",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/tenants/{id}/activate",
        Method::POST,
        "platform.tenant.manage",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/tenants/{id}/suspend",
        Method::POST,
        "platform.tenant.manage",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/tenants/{id}/resume",
        Method::POST,
        "platform.tenant.manage",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/tenants/{id}/archive",
        Method::POST,
        "platform.tenant.manage",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/tenant/settings",
        Method::GET,
        "tenancy.settings.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/settings",
        Method::PUT,
        "tenancy.settings.update",
        ResourceSpec::Tenant,
    ),
];
