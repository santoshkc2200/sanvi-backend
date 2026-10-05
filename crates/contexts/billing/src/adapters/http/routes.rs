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
///
/// Paths group into **one** resource per path carrying all of its methods:
/// actix commits a request to the first matching resource, so registering
/// `/api/v1/platform/plans` twice (GET, then POST) shadowed the POST with
/// a 405 — the defect the per-path grouping below removes (found during
/// TASK-021 11.2c endpoint profiling; regression-tested in this file).
pub fn billing_platform_routes(cfg: &mut web::ServiceConfig) {
    let mut by_path: std::collections::BTreeMap<&'static str, Vec<&super::catalog::RouteDef>> =
        std::collections::BTreeMap::new();
    for route in ROUTES
        .iter()
        .filter(|route| route.path.starts_with("/api/v1/platform"))
    {
        by_path.entry(route.path).or_default().push(route);
    }

    for (path, defs) in by_path {
        let permissions = sanvi_http::PermissionMap::new(
            defs.iter()
                .map(|route| {
                    (
                        route.method.clone(),
                        sanvi_http::RequiredPermission::declared(route.permission, route.resource),
                    )
                })
                .collect(),
        );
        let mut resource = web::resource(path).app_data(permissions);
        for route in defs {
            resource = match (route.method.as_str(), route.path) {
                ("GET", "/api/v1/platform/plans") => resource.route(web::get().to(list_plans)),
                ("POST", "/api/v1/platform/plans") => resource.route(web::post().to(create_plan)),
                ("PATCH", "/api/v1/platform/plans/{id}") => {
                    resource.route(web::patch().to(update_plan))
                }
                ("POST", "/api/v1/platform/plans/{id}/prices") => {
                    resource.route(web::post().to(add_plan_price))
                }
                ("PUT", "/api/v1/platform/plans/{id}/entitlements") => {
                    resource.route(web::put().to(set_plan_entitlements))
                }
                ("POST", "/api/v1/platform/tenants/{id}/subscription/override") => {
                    resource.route(web::post().to(apply_subscription_override))
                }
                ("DELETE", "/api/v1/platform/tenants/{id}/subscription/override") => {
                    resource.route(web::delete().to(revoke_subscription_override))
                }
                (method, path) => {
                    unreachable!("catalog route not registered: {method} {path}")
                }
            };
        }
        cfg.service(resource);
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

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, http::StatusCode, test};

    /// Regression test for the shadowed-method mount defect found during
    /// TASK-021 11.2c endpoint profiling: `POST /api/v1/platform/plans`
    /// answered 405 in the assembled app because its path was registered
    /// twice (GET first, and actix commits to the first matching
    /// resource). Every catalog route must *match* in the composition
    /// mounts — a matched-but-unwired handler (500, no state) passes here
    /// while an unmatched one (404/405) fails naming the operation.
    #[actix_web::test]
    async fn every_catalog_route_matches_in_the_composition_mounts() {
        let app = test::init_service(
            App::new()
                .configure(public_routes)
                .configure(billing_platform_routes)
                .configure(billing_internal_routes)
                .service(web::scope("/api/v1/tenant").configure(tenant_scope_routes)),
        )
        .await;

        for route in ROUTES {
            let req = test::TestRequest::default()
                .method(route.method.clone())
                .uri(route.path)
                .to_request();
            let res = test::call_service(&app, req).await;
            let served = res.status();
            assert_ne!(
                served,
                StatusCode::NOT_FOUND,
                "{} {} is not mounted at its contracted path (assembled-app mount defect)",
                route.method,
                route.path
            );
            assert_ne!(
                served,
                StatusCode::METHOD_NOT_ALLOWED,
                "{} {} is shadowed by another resource on the same path                  (paths must register all methods on one resource)",
                route.method,
                route.path
            );
        }
    }
}
