//! Domain: pure vocabulary for the tenancy context.
//!
//! Nothing here may import `sqlx`, `actix`, `reqwest` or any other
//! infrastructure crate — enforced by `scripts/check-layering.sh` in CI.
//! The `ScopedTx`/`PlatformScope` capability tokens referenced by ports are
//! opaque handles from the persistence crate, not database access.

pub mod error;
pub mod events;
pub mod offboarding;
pub mod ports;
pub mod tenant;
pub mod values;

pub use error::TenancyError;
pub use events::{TenantEvent, TenantEventName};
pub use offboarding::{OffboardingRequest, OffboardingStatus, VerifiedExport};
pub use ports::{
    Clock, EventPublisher, IdGenerator, OffboardingRepository, SlugReservationPort,
    TenantRepository, TenantResolutionCache, TenantSearch, TenantSearchCursor,
    TenantSettingsRepository,
};
pub use tenant::{Tenant, TenantId, TenantStatus, TenantTag, TenantTimestamps};
pub use values::{DataRegion, Locale, SuspensionReason, TenantName, TenantSlug};
