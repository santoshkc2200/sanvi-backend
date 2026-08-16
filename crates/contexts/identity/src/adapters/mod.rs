//! Adapters: infrastructure (driven) and transport (driving).

pub mod events;
pub mod http;
pub mod kratos;
pub mod notifications;
pub mod repo;
pub mod session_cache;
pub mod verifier;

pub use events::{OutboxEventPublisher, SystemClock, SystemIdGenerator};
pub use kratos::{HttpKratosClient, kratos_client_impl};
pub use notifications::LoggingNotifications;
pub use session_cache::{NoopSessionCache, RedisSessionCache};
pub use verifier::KratosSessionVerifier;
