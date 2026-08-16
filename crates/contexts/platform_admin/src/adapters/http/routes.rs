//! Route registration: every route here comes from the catalog, which also
//! feeds the CI permission test — the two can never drift apart.

use actix_web::web;

use super::catalog::ROUTES;
use super::handlers::{
    approve_request, create_impersonation, get_tenant_admin_view, grant_entitlement_override,
    list_entitlement_overrides, list_impersonations, list_pending_approvals, reject_request,
    revoke_entitlement_override, revoke_impersonation, search_tenant_admin_views,
};

pub fn platform_admin_routes(cfg: &mut web::ServiceConfig) {
    for route in ROUTES {
        let permission = sanvi_http::RequiredPermission::declared(route.permission, route.resource);
        let resource = web::resource(route.path).app_data(permission);
        match (route.method.as_str(), route.path) {
            ("GET", "/api/v1/platform/admin/tenants") => {
                cfg.service(resource.route(web::get().to(search_tenant_admin_views)));
            }
            ("GET", "/api/v1/platform/admin/tenants/{id}") => {
                cfg.service(resource.route(web::get().to(get_tenant_admin_view)));
            }
            ("POST", "/api/v1/platform/impersonations") => {
                cfg.service(resource.route(web::post().to(create_impersonation)));
            }
            ("GET", "/api/v1/platform/impersonations") => {
                cfg.service(resource.route(web::get().to(list_impersonations)));
            }
            ("DELETE", "/api/v1/platform/impersonations/{id}") => {
                cfg.service(resource.route(web::delete().to(revoke_impersonation)));
            }
            ("POST", "/api/v1/platform/tenants/{id}/entitlements") => {
                cfg.service(resource.route(web::post().to(grant_entitlement_override)));
            }
            ("GET", "/api/v1/platform/tenants/{id}/entitlements") => {
                cfg.service(resource.route(web::get().to(list_entitlement_overrides)));
            }
            ("DELETE", "/api/v1/platform/tenants/{id}/entitlements/{feature}") => {
                cfg.service(resource.route(web::delete().to(revoke_entitlement_override)));
            }
            ("GET", "/api/v1/platform/approvals") => {
                cfg.service(resource.route(web::get().to(list_pending_approvals)));
            }
            ("POST", "/api/v1/platform/approvals/{id}/approve") => {
                cfg.service(resource.route(web::post().to(approve_request)));
            }
            ("POST", "/api/v1/platform/approvals/{id}/reject") => {
                cfg.service(resource.route(web::post().to(reject_request)));
            }
            (method, path) => {
                unreachable!("catalog route not registered: {method} {path}")
            }
        }
    }
}
