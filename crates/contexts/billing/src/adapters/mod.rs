//! Adapters: infrastructure (driven) and transport (driving).

pub mod events;
pub mod http;
pub mod lifecycle;
pub mod metrics;
pub mod notifications;
pub mod privacy_provider;
pub mod repos;
pub mod stripe;
pub mod sync;

pub use events::{OutboxEventPublisher, SystemClock, SystemIdGenerator};
pub use lifecycle::TenancyTenantLifecycle;
pub use notifications::LoggingNotifications;
pub use repos::{
    SqlxCheckoutStateRepository, SqlxInvoiceRepository, SqlxPlanEntitlementRepository,
    SqlxPlanPriceRepository, SqlxPlanRepository, SqlxSubscriptionRepository,
    SqlxWebhookEventRepository,
};
pub use stripe::{CachedTaxGate, HttpStripeClient, StripeMetrics, verify_webhook_signature};
pub use sync::{AccessEntitlementSyncer, billing_event_handler};
