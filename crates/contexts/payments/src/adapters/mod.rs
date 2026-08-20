//! Adapters: HTTP transport, persistence, outbox publishing, metrics and
//! the provider adapters (the fake provider now; the Stripe Connect
//! adapter arrives with 09.2 behind the same ports).

pub mod events;
pub mod fake_provider;
pub mod http;
pub mod metrics;
pub mod privacy_provider;
pub mod repos;
pub mod stubs;

pub use events::OutboxEventPublisher;
pub use fake_provider::{FAKE_WEBHOOK_SECRET, FakeProvider, FakeScript};
pub use metrics::PaymentsMetrics;
pub use privacy_provider::PaymentsPrivacyProvider;
pub use repos::SqlxProviderConnectionRepository;
pub use stubs::EmptyProviderCatalog;
