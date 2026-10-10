//! Route registration: every route here comes from the catalog, which also
//! feeds the CI permission test — the two can never drift apart.

use std::collections::BTreeMap;

use actix_web::web;

use super::catalog::ROUTES;
use super::handlers::{
    approve_request, create_impersonation, disable_application_fee, get_application_fee,
    get_offboarding, get_tenant_admin_view, grant_entitlement_override, list_entitlement_overrides,
    list_impersonations, list_pending_approvals, reject_request, request_offboarding,
    revoke_entitlement_override, revoke_impersonation, search_tenant_admin_views,
    set_application_fee,
};

/// One `web::resource` per path, with every catalog method merged onto it.
/// Registering the same path twice makes the later resource silently replace
/// the earlier one in actix (the replaced methods answer 405), and the
/// permission `app_data` rides on each *route*, because GET and POST on one
/// path can require different permissions (read vs write).
pub fn platform_admin_routes(cfg: &mut web::ServiceConfig) {
    let mut by_path: BTreeMap<&'static str, Vec<&super::catalog::RouteDef>> = BTreeMap::new();
    for route in ROUTES {
        by_path.entry(route.path).or_default().push(route);
    }

    for (path, routes) in by_path {
        // Per-method permissions ride on one `PermissionMap` for the whole
        // resource: `Route` carries no `app_data` of its own in actix-web 4.
        let permissions = sanvi_http::PermissionMap::new(
            routes
                .iter()
                .map(|route| {
                    (
                        route.method.clone(),
                        sanvi_http::RequiredPermission::declared(route.permission, route.resource),
                    )
                })
                .collect(),
        );
        let mut resource = web::resource(path).app_data(permissions);
        for route in routes {
            resource = match (route.method.as_str(), route.path) {
                ("GET", "/api/v1/platform/admin/tenants") => {
                    resource.route(web::get().to(search_tenant_admin_views))
                }
                ("GET", "/api/v1/platform/admin/tenants/{id}") => {
                    resource.route(web::get().to(get_tenant_admin_view))
                }
                ("POST", "/api/v1/platform/tenants/{id}/offboarding") => {
                    resource.route(web::post().to(request_offboarding))
                }
                ("GET", "/api/v1/platform/tenants/{id}/offboarding") => {
                    resource.route(web::get().to(get_offboarding))
                }
                ("POST", "/api/v1/platform/impersonations") => {
                    resource.route(web::post().to(create_impersonation))
                }
                ("GET", "/api/v1/platform/impersonations") => {
                    resource.route(web::get().to(list_impersonations))
                }
                ("DELETE", "/api/v1/platform/impersonations/{id}") => {
                    resource.route(web::delete().to(revoke_impersonation))
                }
                ("POST", "/api/v1/platform/tenants/{id}/entitlements") => {
                    resource.route(web::post().to(grant_entitlement_override))
                }
                ("GET", "/api/v1/platform/tenants/{id}/entitlements") => {
                    resource.route(web::get().to(list_entitlement_overrides))
                }
                // 09.6: the platform's per-tenant application fee. Read uses
                // the tenant-read permission; changes are entitlement-write
                // gated + fresh-MFA in the handler.
                ("GET", "/api/v1/platform/tenants/{id}/application-fee") => {
                    resource.route(web::get().to(get_application_fee))
                }
                ("PUT", "/api/v1/platform/tenants/{id}/application-fee") => {
                    resource.route(web::put().to(set_application_fee))
                }
                ("DELETE", "/api/v1/platform/tenants/{id}/application-fee") => {
                    resource.route(web::delete().to(disable_application_fee))
                }
                ("DELETE", "/api/v1/platform/tenants/{id}/entitlements/{feature}") => {
                    resource.route(web::delete().to(revoke_entitlement_override))
                }
                ("GET", "/api/v1/platform/approvals") => {
                    resource.route(web::get().to(list_pending_approvals))
                }
                ("POST", "/api/v1/platform/approvals/{id}/approve") => {
                    resource.route(web::post().to(approve_request))
                }
                ("POST", "/api/v1/platform/approvals/{id}/reject") => {
                    resource.route(web::post().to(reject_request))
                }
                (method, path) => {
                    unreachable!("catalog route not registered: {method} {path}")
                }
            };
        }
        cfg.service(resource);
    }
}
