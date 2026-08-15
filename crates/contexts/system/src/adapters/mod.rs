//! Adapters: transport (HTTP) and infrastructure (probes).

pub mod http;
pub mod probes;

pub use probes::{PostgresProbe, RedisProbe};
