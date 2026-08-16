//! Adapters: infrastructure (driven) and transport (driving).

pub mod decision_cache;
pub mod entitlement_cache;
pub mod entitlements_repo;
pub mod events;
pub mod features_repo;
pub mod http;
pub mod keto;
pub mod membership_source;
pub mod role_catalog;
pub mod roles_repo;
pub mod sync;
pub mod system;

pub use decision_cache::{InMemoryDecisionCache, NoopDecisionCache};
pub use entitlement_cache::{
    InMemoryEntitlementCache, NoopEntitlementCache, RedisEntitlementCache, entitlement_key,
};
pub use entitlements_repo::{SqlxEntitlementRepository, SqlxQuotaUsageStore};
pub use events::OutboxEventPublisher;
pub use features_repo::SqlxFeatureRepository;
pub use keto::{HttpKetoClient, KetoCheckError};
pub use membership_source::SqlxMembershipSource;
pub use role_catalog::SqlxRoleCatalog;
pub use roles_repo::SqlxRoleRepository;
pub use system::{SystemClock, SystemRoleIdGenerator};
