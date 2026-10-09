//! HTTP adapter for the localization context: application state, handlers
//! and route registration.

pub mod handlers;
pub mod routes;
pub mod state;
pub mod validation;

pub use handlers::{api_error, get_public_locales, get_public_messages};
pub use routes::{
    PERMISSIONED_ROUTES, localization_platform_routes, localization_public_routes,
    localization_tenant_scope_routes,
};
pub use state::LocalizationState;
