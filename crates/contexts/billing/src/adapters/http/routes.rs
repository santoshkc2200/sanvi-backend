//! Route registration: every route here comes from the catalog, which also
//! feeds the CI permission test — the two can never drift apart. The public
//! pricing page and the internal webhook endpoint are exempt by design.

use actix_web::web;

use super::catalog::ROUTES;
use super::handlers::{
    add_plan_price, apply_subscription_override, create_checkout_session, create_plan,
    create_portal_session, get_subscription, list_invoices, list_plans, list_public_plans,
    revoke_subscription_override, set_plan_entitlements, stripe_platform_webhook, update_plan,
};

/// The public pricing page (no permission requirement).
pub fn public_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/api/v1/public/plans").route(web::get().to(list_public_plans)));
}

/// Tenant-scoped billing endpoints (mounted inside the existing
/// `/api/v1/tenant` scope; paths are registered relative to it).
pub fn tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    for route in ROUTES
        .iter()
        .filter(|route| route.path.starts_with("/api/v1/tenant/billing"))
    {
        let relative = route
            .path
            .strip_prefix("/api/v1/tenant")
            .unwrap_or(route.path);
        let permission = sanvi_http::RequiredPermission::declared(route.permission, route.resource);
        let resource = web::resource(relative).app_data(permission);
        match (route.method.as_str(), route.path) {
            ("POST", "/api/v1/tenant/billing/checkout-session") => {
                cfg.service(resource.route(web::post().to(create_checkout_session)));
            }
            ("POST", "/api/v1/tenant/billing/portal-session") => {
                cfg.service(resource.route(web::post().to(create_portal_session)));
            }
            ("GET", "/api/v1/tenant/billing/subscription") => {
                cfg.service(resource.route(web::get().to(get_subscription)));
            }
            ("GET", "/api/v1/tenant/billing/invoices") => {
                cfg.service(resource.route(web::get().to(list_invoices)));
            }
            (method, path) => {
                unreachable!("catalog route not registered: {method} {path}")
            }
        }
    }
}

/// Platform-scoped billing endpoints.
pub fn billing_platform_routes(cfg: &mut web::ServiceConfig) {
    for route in ROUTES
        .iter()
        .filter(|route| route.path.starts_with("/api/v1/platform"))
    {
        let permission = sanvi_http::RequiredPermission::declared(route.permission, route.resource);
        let resource = web::resource(route.path).app_data(permission);
        match (route.method.as_str(), route.path) {
            ("GET", "/api/v1/platform/plans") => {
                cfg.service(resource.route(web::get().to(list_plans)));
            }
            ("POST", "/api/v1/platform/plans") => {
                cfg.service(resource.route(web::post().to(create_plan)));
            }
            ("PATCH", "/api/v1/platform/plans/{id}") => {
                cfg.service(resource.route(web::patch().to(update_plan)));
            }
            ("POST", "/api/v1/platform/plans/{id}/prices") => {
                cfg.service(resource.route(web::post().to(add_plan_price)));
            }
            ("PUT", "/api/v1/platform/plans/{id}/entitlements") => {
                cfg.service(resource.route(web::put().to(set_plan_entitlements)));
            }
            ("POST", "/api/v1/platform/tenants/{id}/subscription/override") => {
                cfg.service(resource.route(web::post().to(apply_subscription_override)));
            }
            ("DELETE", "/api/v1/platform/tenants/{id}/subscription/override") => {
                cfg.service(resource.route(web::delete().to(revoke_subscription_override)));
            }
            (method, path) => {
                unreachable!("catalog route not registered: {method} {path}")
            }
        }
    }
}

/// The internal Stripe webhook receiver (signature-verified; exempt from
/// authentication by the `/internal` prefix policy).
pub fn billing_internal_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/internal/webhooks/stripe/platform")
            .route(web::post().to(stripe_platform_webhook)),
    );
}
