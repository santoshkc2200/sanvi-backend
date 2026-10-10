//! SQLx repositories.
//!
//! The scoping guard lives in the constructors: tenant-scoped repos accept
//! only `&mut ScopedTx`, platform-wide repos only `&mut PlatformScope` —
//! both obtainable solely through `TxManager::begin_for` / `begin_unscoped`.

mod offboarding_repo;
mod settings_repo;
mod slug_reservations;
mod tenant_repo;

pub use offboarding_repo::SqlxOffboardingRepository;
pub use settings_repo::SqlxTenantSettingsRepository;
pub use slug_reservations::SqlxSlugReservationPort;
pub use tenant_repo::SqlxTenantRepository;
