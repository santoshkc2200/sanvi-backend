//! HTTP transport, repositories, events, metrics and test stubs.

pub mod contract;
pub mod events;
pub mod fx_rate;
pub mod http;
pub mod metrics;
pub mod metrics_repo;
pub mod oauth_state_store;
pub mod payment_lookup;
pub mod privacy_provider;
pub mod rate_limit;
pub mod repos;
pub mod stubs;
pub mod token_vault;
pub mod vendor_role;

pub use events::{OutboxEventPublisher, SystemIdGenerator};
pub use fx_rate::UnavailableFxRate;
pub use metrics::AdvertisingMetrics;
pub use metrics_repo::SqlxMetricsRepository;
pub use oauth_state_store::SqlxOAuthStateStore;
pub use payment_lookup::CheckoutPaymentLookup;
pub use privacy_provider::AdvertisingPrivacyProvider;
pub use rate_limit::{QuotaLimiter, RateLimitedAdapter, rate_limited_registry};
pub use repos::{
    SqlxAdvertisingConnectionRepository, SqlxCampaignRepository, SqlxCreativeRepository,
    SqlxTrackingSettingsRepository,
};
pub use stubs::{FakeAdPlatform, FakeAudienceSync, FakeAudienceSyncCall, FakeConversionSink};
pub use token_vault::{
    SqlxConnectionTokenRepository, SqlxTenantEnvelopeCrypto, SqlxTenantKeyRepository,
};
pub use vendor_role::SnapshotVendorRoleLookup;
