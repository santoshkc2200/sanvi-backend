//! Route registration for the payments context: tenant-scope resources
//! mount inside the shared `/api/v1/tenant` scope, behind the phase-09
//! feature flag and the `payments.stripe_connect` entitlement — except the
//! provider catalog, which is readable with `payments.read` alone so an
//! un-entitled tenant can see what it would be buying.

use actix_web::http::Method;
use actix_web::web;

use sanvi_http::{PermissionMap, ResourceSpec, require_permission};

use super::handlers::{
    create_payment_connection, create_payment_connection_session, create_tenant_checkout,
    delete_payment_connection, export_payments, get_payment, get_payment_connection,
    get_tax_settings, get_tenant_checkout, list_disputes, list_payment_providers, list_payments,
    list_payouts, put_tax_settings, refund_payment,
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
    let refund = require_permission!("payments.refund", tenant);

    cfg.service(
        // The catalog is the one payments route NOT behind the entitlement: it
        // is the menu, not the meal. A tenant without `payments.stripe_connect`
        // must be able to see which providers exist to know what upgrading buys
        // them — the admin renders the cards with the connect CTA replaced by an
        // upgrade prompt. Connecting stays gated below, which is where money
        // actually starts moving. `payments.read` still applies, so this widens
        // the audience to un-entitled tenants, never to unauthorised ones.
        web::resource("/payments/providers")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
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
            .app_data(PermissionMap::new(vec![(Method::GET, read.clone()), (Method::DELETE, manage)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(get_payment_connection))
            // 09.7: disconnect — guarded, idempotent, local-only.
            .route(web::delete().to(delete_payment_connection)),
    )
    .service(
        web::resource("/payments/connections/{id}/session")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::post().to(create_payment_connection_session)),
    )
    // 09.5: payment read model, refunds & disputes.
    .service(
        web::resource("/payments")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(list_payments)),
    )
    // 09.6: payout visibility + tax settings. Registered before the
    // `/payments/{id}` catch-all so "payouts" / "tax-settings" are never
    // parsed as a payment id.
    .service(
        web::resource("/payments/payouts")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(list_payouts)),
    )
    .service(
        web::resource("/payments/tax-settings")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read.clone()),
                (Method::PUT, manage),
            ]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(get_tax_settings))
            .route(web::put().to(put_tax_settings)),
    )
    .service(
        web::resource("/payments/export")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(export_payments)),
    )
    .service(
        web::resource("/payments/disputes")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(list_disputes)),
    )
    .service(
        web::resource("/payments/{id}")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(get_payment)),
    )
    .service(
        web::resource("/payments/{id}/refund")
            .app_data(PermissionMap::new(vec![(Method::POST, refund)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::post().to(refund_payment)),
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
        "/api/v1/tenant/payments/connections/{id}",
        Method::DELETE,
        "payments.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/connections/{id}/session",
        Method::POST,
        "payments.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments",
        Method::GET,
        "payments.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/export",
        Method::GET,
        "payments.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/disputes",
        Method::GET,
        "payments.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/payouts",
        Method::GET,
        "payments.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/tax-settings",
        Method::GET,
        "payments.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/tax-settings",
        Method::PUT,
        "payments.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/{id}",
        Method::GET,
        "payments.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/payments/{id}/refund",
        Method::POST,
        "payments.refund",
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
