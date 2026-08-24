//! Advertising context (phase 10).
//!
//! TASK-009 deliberately exposes only the entitlement-gated empty platform
//! catalog. It also establishes secret-safe vocabulary and configuration so
//! platform adapters cannot arrive before their safety boundaries.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::AdvertisingConfig;
pub use domain::{
    CapabilityMatrix, ConnectionHandle, ConsentSnapshot, HashedIds, Objective, PlatformKey,
    SecretRef,
};

pub mod routes {
    pub use crate::adapters::http::routes::advertising_tenant_scope_routes;
}

pub mod catalog {
    pub use crate::adapters::http::routes::{FEATURE_GATED_ROUTES, PERMISSIONED_ROUTES};
}
