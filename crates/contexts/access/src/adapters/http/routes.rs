//! Route registration: every route here comes from the catalog, which also
//! feeds the CI permission/feature tests — the three can never drift apart.
//!
//! One `web::resource` per path throughout: registering the same path twice
//! makes the later resource silently replace the earlier one in actix, and
//! every method the replaced resource carried answers 405. Shared paths
//! (feature catalog GET+POST, PATCH+DELETE) therefore merge their methods.

use std::collections::BTreeMap;

use actix_web::web;

use super::catalog::ROUTES;
use super::handlers::{
    create_role, define_feature, delete_role, deprecate_feature, list_features, list_permissions,
    list_roles_platform, list_roles_tenant, list_tenant_entitlements, update_feature, update_role,
};

/// App-level routes — everything NOT under `/api/v1/tenant`.
///
/// The `/api/v1/tenant/*` slice lives in [`tenant_scope_routes`]: the API
/// mounts a scope on that prefix, and a scope shadows App-level resources,
/// so registering tenant routes here would make them unreachable (404).
pub fn access_routes(cfg: &mut web::ServiceConfig) {
    let mut by_path: BTreeMap<&'static str, Vec<&super::catalog::RouteDef>> = BTreeMap::new();
    for route in ROUTES {
        if route.path.starts_with("/api/v1/tenant/") {
            continue;
        }
        by_path.entry(route.path).or_default().push(route);
    }

    for (path, routes) in by_path {
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
        for route in &routes {
            if let Some(feature) = route.feature {
                resource = resource.app_data(sanvi_http::RequiredFeature::declared(feature));
            }
        }
        for route in routes {
            resource = match (route.method.as_str(), route.path) {
                ("GET", "/api/v1/access/permissions") => {
                    resource.route(web::get().to(list_permissions))
                }
                ("GET", "/api/v1/platform/roles") => {
                    resource.route(web::get().to(list_roles_platform))
                }
                ("GET", "/api/v1/platform/features") => {
                    resource.route(web::get().to(list_features))
                }
                ("POST", "/api/v1/platform/features") => {
                    resource.route(web::post().to(define_feature))
                }
                ("PATCH", "/api/v1/platform/features/{key}") => {
                    resource.route(web::patch().to(update_feature))
                }
                ("DELETE", "/api/v1/platform/features/{key}") => {
                    resource.route(web::delete().to(deprecate_feature))
                }
                (method, path) => {
                    unreachable!("catalog route not registered: {method} {path}")
                }
            };
        }
        cfg.service(resource);
    }
}

/// The `/api/v1/tenant/*` slice, configured *inside* the shared tenant
/// scope — paths are relative to the scope's prefix.
pub fn tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let mut by_path: BTreeMap<&'static str, Vec<&super::catalog::RouteDef>> = BTreeMap::new();
    for route in ROUTES {
        if !route.path.starts_with("/api/v1/tenant/") {
            continue;
        }
        let path = route.path.trim_start_matches("/api/v1/tenant");
        by_path.entry(path).or_default().push(route);
    }

    for (path, routes) in by_path {
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
        for route in &routes {
            if let Some(feature) = route.feature {
                resource = resource.app_data(sanvi_http::RequiredFeature::declared(feature));
            }
        }
        for route in routes {
            resource = match (route.method.as_str(), path) {
                ("GET", "/roles") => resource.route(web::get().to(list_roles_tenant)),
                ("POST", "/roles") => resource.route(web::post().to(create_role)),
                ("GET", "/entitlements") => resource.route(web::get().to(list_tenant_entitlements)),
                ("PATCH", "/roles/{id}") => resource.route(web::patch().to(update_role)),
                ("DELETE", "/roles/{id}") => resource.route(web::delete().to(delete_role)),
                (method, path) => {
                    unreachable!("catalog tenant route not registered: {method} {path}")
                }
            };
        }
        cfg.service(resource);
    }
}

#[cfg(test)]
mod tests {
    use actix_web::{App, http::StatusCode, test, web};

    use super::{ROUTES, access_routes, tenant_scope_routes};

    /// Every catalog method must be reachable. Registering the same path
    /// twice makes actix keep only the first resource, so the methods on
    /// the shadowed one answer 405 — the failure this guards against.
    /// Without app data the handlers answer 500, which is fine here: the
    /// assertion is only that routing matched at all.
    #[actix_web::test]
    async fn every_catalog_method_is_reachable() {
        let app = test::init_service(
            App::new()
                .configure(access_routes)
                .service(web::scope("/api/v1/tenant").configure(tenant_scope_routes)),
        )
        .await;

        for route in ROUTES {
            let response = test::call_service(
                &app,
                test::TestRequest::default()
                    .method(route.method.clone())
                    .uri(route.path)
                    .to_request(),
            )
            .await;
            assert_ne!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{} {} is shadowed by another registration",
                route.method,
                route.path
            );
            assert_ne!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{} {} is not registered",
                route.method,
                route.path
            );
        }
    }
}
