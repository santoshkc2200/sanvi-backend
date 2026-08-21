//! Adapters: HTTP transport, persistence, outbox publishing, metrics and
//! the provider adapters — the fake provider (non-production) and, from
//! 09.2, the Stripe Connect adapter (Accounts v2), both behind the same
//! ports.

pub mod events;
pub mod fake_provider;
pub mod http;
pub mod metrics;
pub mod notify;
pub mod privacy_provider;
pub mod repos;
pub mod stripe_connect;
pub mod stubs;
pub mod throttle;

pub use events::OutboxEventPublisher;
pub use fake_provider::{FAKE_WEBHOOK_SECRET, FakeProvider, FakeScript};
pub use metrics::PaymentsMetrics;
pub use notify::{DenyAllVerifier, LoggingConnectionNotifier};
pub use privacy_provider::PaymentsPrivacyProvider;
pub use repos::{
    SqlxCheckoutRepository, SqlxProviderConnectionRepository, SqlxWebhookEventRepository,
};
pub use stripe_connect::StripeConnectProvider;
pub use stubs::EmptyProviderCatalog;
pub use throttle::FixedWindowThrottle;
