//! Adapters: infrastructure (driven) and transport (driving).

pub mod events;
pub mod http;
pub mod privacy_provider;
pub mod repo;
pub mod resolution;

pub use events::{OutboxEventPublisher, SystemClock, SystemIdGenerator, tenant_event_handler};
pub use repo::{SqlxSlugReservationPort, SqlxTenantRepository, SqlxTenantSettingsRepository};
pub use resolution::{
    HostTenantResolver, NoopTenantResolutionCache, RedisTenantResolutionCache, SignedTokenError,
    SigningKey, tenant_context_from,
};
