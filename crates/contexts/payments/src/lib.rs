//! The platform side of Payments. The bounded context itself runs in the
//! standalone `payments-service` with its own database; this crate holds
//! what the platform needs to front it: the request-bound delegation and
//! gateway, the published HTTP contract and route permissions, the remote
//! privacy provider, and the shape of the events the service delivers.

pub mod adapters;
pub mod domain;

pub mod routes {
    pub use crate::adapters::http::routes::payments_tenant_scope_routes;
}

pub mod catalog {
    pub use crate::adapters::http::routes::PERMISSIONED_ROUTES;
}
