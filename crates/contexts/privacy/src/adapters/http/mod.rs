//! The privacy context's HTTP layer: handlers, routes and the catalog that
//! feeds the CI permission test.

pub mod catalog;
pub mod handlers;
pub mod routes;
pub mod validation;

pub use handlers::{PrivacyState, api_error};
pub use routes::{
    privacy_platform_routes, privacy_routes, privacy_scope, public_routes, tenant_scope_routes,
};
