//! The access context's route catalog: every route it mounts, with its
//! permission requirement and (optional) feature gate. The "no route
//! without a permission" and "every feature-gated route references a
//! declared feature" CI tests enumerate this list.

use actix_web::http::Method;
use sanvi_http::ResourceSpec;

pub struct RouteDef {
    pub method: Method,
    pub path: &'static str,
    pub permission: &'static str,
    pub resource: ResourceSpec,
    /// Entitlement-gated route (`#[feature("...")]` equivalent): the
    /// extractor denies when the current tenant lacks the feature.
    pub feature: Option<&'static str>,
}

pub const ROUTES: &[RouteDef] = &[
    RouteDef {
        method: Method::GET,
        path: "/api/v1/access/permissions",
        permission: "access.permission.list",
        resource: ResourceSpec::User,
        feature: None,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/tenant/roles",
        permission: "access.role.read",
        resource: ResourceSpec::Tenant,
        feature: None,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/tenant/entitlements",
        permission: "tenant.entitlement.read",
        resource: ResourceSpec::Tenant,
        // The declarative feature gate: this route is only served to
        // tenants with `access.tenant_entitlements` enabled.
        feature: Some("access.tenant_entitlements"),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/tenant/roles",
        permission: "access.role.create",
        resource: ResourceSpec::Tenant,
        feature: None,
    },
    RouteDef {
        method: Method::PATCH,
        path: "/api/v1/tenant/roles/{id}",
        permission: "access.role.update",
        resource: ResourceSpec::Tenant,
        feature: None,
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/tenant/roles/{id}",
        permission: "access.role.delete",
        resource: ResourceSpec::Tenant,
        feature: None,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/roles",
        permission: "platform.role.manage",
        resource: ResourceSpec::Platform,
        feature: None,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/features",
        permission: "platform.feature.write",
        resource: ResourceSpec::Platform,
        feature: None,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/features",
        permission: "platform.feature.write",
        resource: ResourceSpec::Platform,
        feature: None,
    },
    RouteDef {
        method: Method::PATCH,
        path: "/api/v1/platform/features/{key}",
        permission: "platform.feature.write",
        resource: ResourceSpec::Platform,
        feature: None,
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/platform/features/{key}",
        permission: "platform.feature.write",
        resource: ResourceSpec::Platform,
        feature: None,
    },
];
