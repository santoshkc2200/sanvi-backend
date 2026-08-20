//! Route registration for the payments context: tenant-scope resources
//! mount inside the shared `/api/v1/tenant` scope, behind the phase-09
//! feature flag and the `payments.stripe_connect` entitlement.

use actix_web::http::Method;
use actix_web::web;

use sanvi_http::{PermissionMap, ResourceSpec, require_permission};

use super::handlers::list_payment_providers;

/// The entitlement every payments route is gated on (403, not 404 — the
/// frontend renders an upgrade prompt from it).
pub const PAYMENTS_ENTITLEMENT: &str = "payments.stripe_connect";

/// Mounts inside the shared `/api/v1/tenant` scope.
pub fn payments_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("payments.read", tenant);

    cfg.service(
        web::resource("/payments/providers")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredFeature::declared(PAYMENTS_ENTITLEMENT))
            .route(web::get().to(list_payment_providers)),
    );
}

/// The non-public payments routes with their permission requirements —
/// enumerated by the permission catalog CI test.
pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[(
    "/api/v1/tenant/payments/providers",
    Method::GET,
    "payments.read",
    ResourceSpec::Tenant,
)];
