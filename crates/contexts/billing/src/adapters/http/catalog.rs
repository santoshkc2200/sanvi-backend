//! The billing context's route catalog: every permissioned route it mounts,
//! with its permission requirement. The "no route without a permission" CI
//! test enumerates this. The public pricing page and the internal webhook
//! endpoint are deliberately absent: they are public/internal by design
//! (registered alongside the other contexts' public/internal routes).

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
        method: Method::POST,
        path: "/api/v1/tenant/billing/checkout-session",
        permission: "billing.subscription.create",
        resource: ResourceSpec::Tenant,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/tenant/billing/portal-session",
        permission: "billing.subscription.update",
        resource: ResourceSpec::Tenant,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/tenant/billing/subscription",
        permission: "billing.subscription.read",
        resource: ResourceSpec::Tenant,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/tenant/billing/invoices",
        permission: "billing.invoice.read",
        resource: ResourceSpec::Tenant,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/plans",
        permission: "platform.plan.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/plans",
        permission: "platform.plan.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::PATCH,
        path: "/api/v1/platform/plans/{id}",
        permission: "platform.plan.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/plans/{id}/prices",
        permission: "platform.plan.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::PUT,
        path: "/api/v1/platform/plans/{id}/entitlements",
        permission: "platform.plan.write",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/tenants/{id}/subscription/override",
        permission: "platform.subscription.override",
        resource: ResourceSpec::Platform,
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/platform/tenants/{id}/subscription/override",
        permission: "platform.subscription.override",
        resource: ResourceSpec::Platform,
    },
];
