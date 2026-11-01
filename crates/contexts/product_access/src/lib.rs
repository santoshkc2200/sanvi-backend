pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::ProductAccessConfig;

pub mod routes {
    pub use crate::adapters::http::routes::{
        product_access_binding_routes, product_access_public_routes,
        product_access_tenant_scope_routes,
    };
}
