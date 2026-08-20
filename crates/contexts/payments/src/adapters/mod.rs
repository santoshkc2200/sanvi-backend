//! Adapters: HTTP transport, persistence, outbox publishing, metrics and
//! the sandbox/stub gateways later slices swap for live Stripe adapters.

pub mod events;
pub mod http;
pub mod metrics;
pub mod repos;
pub mod stubs;

pub use events::OutboxEventPublisher;
pub use metrics::PaymentsMetrics;
pub use repos::SqlxProviderConnectionRepository;
pub use stubs::EmptyProviderCatalog;
