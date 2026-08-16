//! The billing context's HTTP transport: route registration, handlers and
//! the permission catalog.

pub mod catalog;
pub mod handlers;
pub mod routes;

pub use handlers::BillingState;
pub use routes::{
    billing_internal_routes, billing_platform_routes, public_routes, tenant_scope_routes,
};
