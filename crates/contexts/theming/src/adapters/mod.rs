//! The theming context's adapters: repositories, caches, storage, metrics,
//! event plumbing, HTTP delivery and the privacy provider.

pub mod cache;
pub mod events;
pub mod http;
pub mod metrics;
pub mod privacy_provider;
pub mod repos;
pub mod resolver;
pub mod storage;

pub use cache::ResolvedThemeCache;
pub use events::{OutboxEventPublisher, SystemClock, theming_event_handler};
pub use metrics::ThemingMetrics;
pub use repos::{SqlxTenantThemeRepository, SqlxThemeRepository};
pub use resolver::HostTenantResolverAdapter;
pub use storage::LocalFileStore;
