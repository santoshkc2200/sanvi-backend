//! HTTP driving adapter: the platform audit query.

pub mod catalog;
mod handlers;
pub mod routes;

pub use handlers::{__path_list_audit, AuditState, list_audit};
pub use routes::audit_routes;
