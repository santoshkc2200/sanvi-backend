//! The audit context's route catalog: every route it mounts, with its
//! permission requirement. The "no route without a permission" CI test
//! enumerates this and proves every key is declared in the central registry.

use actix_web::http::Method;
use sanvi_http::ResourceSpec;

pub struct RouteDef {
    pub method: Method,
    pub path: &'static str,
    pub permission: &'static str,
    pub resource: ResourceSpec,
}

pub const ROUTES: &[RouteDef] = &[RouteDef {
    method: Method::GET,
    path: "/api/v1/platform/audit",
    permission: "platform.audit.read",
    resource: ResourceSpec::Platform,
}];
