//! HTTP driving adapter: handlers + routes.

pub mod catalog;
mod handlers;
pub mod routes;

pub use handlers::{__path_list_permissions, AccessState, list_permissions};
pub use routes::access_routes;
