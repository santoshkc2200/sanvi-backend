//! Domain: pure vocabulary for the access context.

pub mod entitlements;
pub mod error;
pub mod events;
pub mod features;
pub mod permissions;
pub mod ports;
pub mod quota;
pub mod roles;

pub use entitlements::{
    Entitlement, EntitlementSource, ResolvedEntitlement, resolve_all, resolve_entitlement,
};
pub use error::AccessError;
pub use events::{AccessEvent, AccessEventName};
pub use features::{
    FEATURE_SEEDS, Feature, FeatureKey, FeatureKind, FeatureSeed, Visibility, validate_seeds,
};
pub use permissions::{PERMISSIONS, Permission, PermissionDef, PermissionScope, lookup};
pub use ports::{
    ApprovalGate, ApprovalOutcome, Authorizer, CachedEntitlements, Clock, Decision, DecisionCache,
    EntitlementCache, EntitlementRepository, EventPublisher, FeatureGate, FeatureRepository,
    IdGenerator, PermissionStore, QuotaUsageStore, ResourceRef, RoleRepository, Tuple,
};
pub use quota::period_start;
pub use roles::{Role, RoleId, RoleScope, SYSTEM_ROLE_SEEDS};
