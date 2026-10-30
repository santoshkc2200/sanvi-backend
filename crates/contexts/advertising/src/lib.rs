//! Platform gateway for the independently deployed Advertising service.
//! Advertising owns its database, use cases and background jobs in sanvi-advertising.
pub mod adapters;
pub mod routes {
    pub use crate::adapters::http::routes::{
        advertising_platform_routes, advertising_tenant_scope_routes, tracking_public_routes,
        tracking_tenant_routes,
    };
}
pub mod catalog {
    pub use crate::adapters::http::routes::{FEATURE_GATED_ROUTES, PERMISSIONED_ROUTES};
}
