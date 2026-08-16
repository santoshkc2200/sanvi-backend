//! Adapters: transport (HTTP) and infrastructure (probes).

pub mod http;
pub mod privacy_provider;
pub mod probes;

pub use privacy_provider::SystemPrivacyProvider;
pub use probes::{PostgresProbe, RedisProbe};
