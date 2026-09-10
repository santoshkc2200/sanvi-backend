//! HTTP transport, repositories, events, metrics and test stubs.

pub mod budget_repo;
pub mod contract;
pub mod events;
pub mod fx_rate;
pub mod google;
pub mod http;
pub mod meta;
pub mod metrics;
pub mod metrics_repo;
pub mod notification_intent;
pub mod oauth_state_store;
pub mod payment_lookup;
pub mod platform_registry;
pub mod privacy_provider;
pub mod rate_limit;
pub mod repos;
pub mod stubs;
pub mod token_vault;
pub mod vendor_role;

pub use budget_repo::{SqlxBudgetAlertRepository, SqlxBudgetCapRepository};
pub use events::{OutboxEventPublisher, SystemIdGenerator};
pub use fx_rate::UnavailableFxRate;
pub use google::{GoogleAdsAdapter, GoogleRefreshTokenGrant};
pub use meta::{MetaAdsAdapter, MetaRefreshTokenGrant};
pub use metrics::AdvertisingMetrics;
pub use metrics_repo::SqlxMetricsRepository;
pub use notification_intent::notification_intent;
pub use oauth_state_store::SqlxOAuthStateStore;
pub use payment_lookup::CheckoutPaymentLookup;
pub use platform_registry::{
    build_access_token_providers, build_audience_sinks, build_conversion_sinks,
    build_platform_registry,
};
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
