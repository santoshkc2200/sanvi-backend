//! Route registration: every route here comes from the catalog, which also
//! feeds the CI permission test — the two can never drift apart.

use actix_web::web;

use super::catalog::ROUTES;
use super::handlers::list_audit;

pub fn audit_routes(cfg: &mut web::ServiceConfig) {
    for route in ROUTES {
        let permission = sanvi_http::RequiredPermission::declared(route.permission, route.resource);
        let resource = web::resource(route.path).app_data(permission);
        match (route.method.as_str(), route.path) {
            ("GET", "/api/v1/platform/audit") => {
                cfg.service(resource.route(web::get().to(list_audit)));
            }
            (method, path) => {
                unreachable!("catalog route not registered: {method} {path}")
            }
        }
    }
}
