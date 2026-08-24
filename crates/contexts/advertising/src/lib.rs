//! Advertising context (phase 10).
//!
//! TASK-009 establishes the entitlement-gated catalog and secret-safe
//! vocabulary. TASK-011 adds the provider-neutral, signed OAuth-state
//! boundary before any callback can exchange a provider authorization code.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::AdvertisingConfig;
pub use domain::{
    AdvertisingConnection, AdvertisingConnectionRepository, CapabilityMatrix, ConnectionHandle,
    ConnectionHealth, ConnectionStatus, ConsentSnapshot, HashedIds, InMemoryOAuthStateStore,
    OAuthStateClaims, OAuthStateError, OAuthStateManager, OAuthStateStore, Objective, PlatformKey,
    RefreshFailureKind, RefreshTransition, SecretRef, SyncError, refresh_failure_transition,
};

pub mod routes {
    pub use crate::adapters::http::routes::advertising_tenant_scope_routes;
}

pub mod catalog {
    pub use crate::adapters::http::routes::{FEATURE_GATED_ROUTES, PERMISSIONED_ROUTES};
}
