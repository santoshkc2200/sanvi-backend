//! The platform_admin context's route catalog: every route it mounts, with
//! its permission requirement. The "no route without a permission" CI test
//! enumerates this.

use actix_web::http::Method;
use sanvi_http::ResourceSpec;

pub struct RouteDef {
    pub method: Method,
    pub path: &'static str,
    pub permission: &'static str,
    pub resource: ResourceSpec,
}

pub const ROUTES: &[RouteDef] = &[
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/admin/tenants",
        permission: "platform.tenant.read",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/admin/tenants/{id}",
        permission: "platform.tenant.read",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/impersonations",
        permission: "platform.impersonate",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/impersonations",
        permission: "platform.impersonate",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/platform/impersonations/{id}",
        permission: "platform.impersonate",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/tenants/{id}/entitlements",
        permission: "platform.entitlement.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/tenants/{id}/entitlements",
        permission: "platform.entitlement.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/tenants/{id}/application-fee",
        permission: "platform.tenant.read",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::PUT,
        path: "/api/v1/platform/tenants/{id}/application-fee",
        permission: "platform.entitlement.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/platform/tenants/{id}/application-fee",
        permission: "platform.entitlement.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/platform/tenants/{id}/entitlements/{feature}",
        permission: "platform.entitlement.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/approvals",
        permission: "platform.approval.read",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/approvals/{id}/approve",
        permission: "platform.approval.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/approvals/{id}/reject",
        permission: "platform.approval.write",
        resource: ResourceSpec::Platform,
    },
];
