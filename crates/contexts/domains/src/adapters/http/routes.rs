//! Route registration for the domains context: tenant-scope resources
//! mount inside the shared `/api/v1/tenant` scope; platform and internal
//! routes mount separately, all behind the phase-08 feature flag.

use actix_web::http::Method;
use actix_web::web;

use sanvi_http::{PermissionMap, ResourceSpec, require_permission};

use super::handlers::{
    claim_custom_domain, get_domain_instructions, list_custom_domains, list_domain_orders,
    list_platform_domains, place_domain_order, promote_primary_domain, release_platform_domain,
    remove_custom_domain, request_domain_verification, search_domains, set_order_auto_renew,
    tls_authorize,
};

/// Mounts inside the shared `/api/v1/tenant` scope.
pub fn domains_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let manage = require_permission!("tenant.domains.manage", tenant);
    let read = require_permission!("tenant.domains.read", tenant);
    let purchase = require_permission!("tenant.domains.purchase", tenant);

    cfg.service(
        web::resource("/domains")
            .app_data(PermissionMap::new(vec![
                (Method::POST, manage),
                (Method::GET, read),
            ]))
            .route(web::post().to(claim_custom_domain))
            .route(web::get().to(list_custom_domains)),
    )
    .service(
        web::resource("/domains/search")
            .app_data(purchase)
            .route(web::get().to(search_domains)),
    )
    .service(
        web::resource("/domains/orders")
            .app_data(PermissionMap::new(vec![
                (Method::POST, purchase),
                (Method::GET, purchase),
            ]))
            .route(web::post().to(place_domain_order))
            .route(web::get().to(list_domain_orders)),
    )
    .service(
        web::resource("/domains/orders/{id}/auto-renew")
            .app_data(purchase)
            .route(web::post().to(set_order_auto_renew)),
    )
    .service(
        web::resource("/domains/{id}/instructions")
            .app_data(read)
            .route(web::get().to(get_domain_instructions)),
    )
    .service(
        web::resource("/domains/{id}/verify")
            .app_data(manage)
            .route(web::post().to(request_domain_verification)),
    )
    .service(
        web::resource("/domains/{id}/promote")
            .app_data(manage)
            .route(web::post().to(promote_primary_domain)),
    )
    .service(
        web::resource("/domains/{id}")
            .app_data(manage)
            .route(web::delete().to(remove_custom_domain)),
    );
}

/// Platform (operator) routes.
pub fn domains_platform_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("platform.domain.read", platform);
    let write = require_permission!("platform.domain.write", platform);
    cfg.service(
        web::resource("/api/v1/platform/domains")
            .app_data(read)
            .route(web::get().to(list_platform_domains)),
    )
    .service(
        web::resource("/api/v1/platform/domains/{id}/release")
            .app_data(write)
            .route(web::post().to(release_platform_domain)),
    );
}

/// Internal routes: the edge's TLS authorization gate (self-authenticated).
pub fn domains_internal_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/internal/tls/authorize").route(web::get().to(tls_authorize)));
}

/// The non-public domains routes with their permission requirements —
/// enumerated by the permission catalog CI test.
pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[
    (
        "/api/v1/tenant/domains",
        Method::POST,
        "tenant.domains.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains",
        Method::GET,
        "tenant.domains.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains/search",
        Method::GET,
        "tenant.domains.purchase",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains/orders",
        Method::POST,
        "tenant.domains.purchase",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains/orders",
        Method::GET,
        "tenant.domains.purchase",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains/orders/{id}/auto-renew",
        Method::POST,
        "tenant.domains.purchase",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains/{id}/instructions",
        Method::GET,
        "tenant.domains.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains/{id}/verify",
        Method::POST,
        "tenant.domains.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains/{id}/promote",
        Method::POST,
        "tenant.domains.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/domains/{id}",
        Method::DELETE,
        "tenant.domains.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/platform/domains",
        Method::GET,
        "platform.domain.read",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/domains/{id}/release",
        Method::POST,
        "platform.domain.write",
        ResourceSpec::Platform,
    ),
];
