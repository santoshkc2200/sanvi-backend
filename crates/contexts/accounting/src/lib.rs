//! The platform side of Accounting. The bounded context itself runs in the
//! standalone `accounting-service` with its own database; this crate holds
//! what the platform needs to front it: the request-bound delegation and
//! gateway, the published HTTP contract and route permissions, and the remote
//! privacy provider.

pub mod adapters;
pub mod domain;

pub mod routes {
    pub use crate::adapters::http::routes::accounting_tenant_scope_routes;
}
