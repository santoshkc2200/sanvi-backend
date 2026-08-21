//! Route registration for the payments context: tenant-scope resources
//! mount inside the shared `/api/v1/tenant` scope, behind the phase-09
//! feature flag and the `payments.stripe_connect` entitlement.

use actix_web::http::Method;
use actix_web::web;

use sanvi_http::{PermissionMap, ResourceSpec, require_permission};

use super::handlers::{
    create_payment_connection, create_payment_connection_session, create_tenant_checkout,
    get_payment_connection, get_tenant_checkout, list_payment_providers,
};

/// The entitlement every payments route is gated on (403, not 404 — the
/// frontend renders an upgrade prompt from it).
pub const PAYMENTS_ENTITLEMENT: &str = "payments.stripe_connect";

/// The permission gating checkout — the storefront's buy path (the same
/// key the release flag is named after).
pub const CHECKOUT_PERMISSION: &str = "payments.checkout";

/// Mounts inside the shared `/api/v1/tenant` scope.
pub fn payments_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("payments.read", tenant);
    let manage = require_permission!("payments.manage", tenant);
    let checkout = require_permission!("payments.checkout", tenant);

    cfg.service(
        web::resource("/payments/providers")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(list_payment_providers)),
    )
    .service(
        web::resource("/payments/connections")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::post().to(create_payment_connection)),
    )
    .service(
        web::resource("/payments/connections/{id}")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(get_payment_connection)),
    )
    .service(
        web::resource("/payments/connections/{id}/session")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::post().to(create_payment_connection_session)),
    )
    // 09.4: the buy path. Same scope as every payments route; the public
    // storefront-scoped variant arrives with the storefront integration
    // and will carry its own rate limiting (this one is throttled in the
    // handler per tenant / IP / customer reference already).
    .service(
        web::resource("/checkout")
            .app_data(PermissionMap::new(vec![(Method::POST, checkout)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::post().to(create_tenant_checkout)),
    )
    .service(
        web::resource("/checkout/{id}")
            .app_data(PermissionMap::new(vec![(Method::GET, checkout)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(get_tenant_checkout)),
    );
}

/// The non-public payments routes with their permission requirements —
/// enumerated by the permission catalog CI test.
pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[
    (
        "/api/v1/tenant/payments/providers",
        Method::GET,
        "payments.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/connections",
        Method::POST,
        "payments.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/connections/{id}",
        Method::GET,
        "payments.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/connections/{id}/session",
        Method::POST,
        "payments.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/checkout",
        Method::POST,
        "payments.checkout",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/checkout/{id}",
        Method::GET,
        "payments.checkout",
        ResourceSpec::Tenant,
    ),
];
