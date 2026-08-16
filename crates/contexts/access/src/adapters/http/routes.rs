//! Route registration: every route here comes from the catalog, which also
//! feeds the CI permission/feature tests — the three can never drift apart.

use actix_web::web;

use super::catalog::ROUTES;
use super::handlers::{
    create_role, define_feature, delete_role, deprecate_feature, list_features, list_permissions,
    list_roles_platform, list_roles_tenant, list_tenant_entitlements, update_feature, update_role,
};

pub fn access_routes(cfg: &mut web::ServiceConfig) {
    for route in ROUTES {
        let permission = sanvi_http::RequiredPermission::declared(route.permission, route.resource);
        let mut resource = web::resource(route.path).app_data(permission);
        if let Some(feature) = route.feature {
            resource = resource.app_data(sanvi_http::RequiredFeature::declared(feature));
        }
        match (route.method.as_str(), route.path) {
            ("GET", "/api/v1/access/permissions") => {
                cfg.service(resource.route(web::get().to(list_permissions)));
            }
            ("GET", "/api/v1/tenant/roles") => {
                cfg.service(resource.route(web::get().to(list_roles_tenant)));
            }
            ("GET", "/api/v1/tenant/entitlements") => {
                cfg.service(resource.route(web::get().to(list_tenant_entitlements)));
            }
            ("POST", "/api/v1/tenant/roles") => {
                cfg.service(resource.route(web::post().to(create_role)));
            }
            ("PATCH", "/api/v1/tenant/roles/{id}") => {
                cfg.service(resource.route(web::patch().to(update_role)));
            }
            ("DELETE", "/api/v1/tenant/roles/{id}") => {
                cfg.service(resource.route(web::delete().to(delete_role)));
            }
            ("GET", "/api/v1/platform/roles") => {
                cfg.service(resource.route(web::get().to(list_roles_platform)));
            }
            ("GET", "/api/v1/platform/features") => {
                cfg.service(resource.route(web::get().to(list_features)));
            }
            ("POST", "/api/v1/platform/features") => {
                cfg.service(resource.route(web::post().to(define_feature)));
            }
            ("PATCH", "/api/v1/platform/features/{key}") => {
                cfg.service(resource.route(web::patch().to(update_feature)));
            }
            ("DELETE", "/api/v1/platform/features/{key}") => {
                cfg.service(resource.route(web::delete().to(deprecate_feature)));
            }
            (method, path) => {
                unreachable!("catalog route not registered: {method} {path}")
            }
        }
    }
}
