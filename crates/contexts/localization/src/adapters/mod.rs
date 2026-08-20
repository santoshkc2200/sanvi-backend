//! Adapters for the localization context: catalog resolution, caches,
//! metrics, SQLx repositories, event publishing, negotiation middleware
//! resolver and HTTP handlers.

pub mod cache;
pub mod catalog;
pub mod events;
pub mod http;
pub mod metrics;
pub mod notifications;
pub mod privacy_provider;
pub mod repos;
pub mod resolver;

pub use cache::LocalizationCache;
pub use catalog::LocalizationService;
pub use events::{OutboxEventPublisher, SystemClock};
pub use metrics::LocalizationMetrics;
pub use notifications::{NotificationLocale, NotificationLocalizer};
pub use privacy_provider::LocalizationPrivacyProvider;
pub use repos::{
    PoolUserLocaleProvider, SqlxSupportedLocaleRepository, SqlxTenantLocaleRepository,
    SqlxTranslationRepository,
};
pub use resolver::{LocaleResolverService, default_timezone_for};
