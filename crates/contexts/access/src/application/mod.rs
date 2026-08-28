//! Application: use cases for the access context.

pub mod check_permission;
pub mod entitlements;
pub mod feature_gate;
pub mod features;
pub mod list_permissions;
pub mod rebuild_tuples;
pub mod roles;
pub mod seed_features;
pub mod seed_roles;

pub use check_permission::{CachedAuthorizer, CheckPermission};
pub use entitlements::{
    ExpireEntitlements, GrantEntitlement, GrantEntitlementCommand, GrantEntitlementOutput,
    ListTenantEntitlements, ListTenantOverrides, RevokeEntitlement, RevokeEntitlementCommand,
};
pub use feature_gate::{EntitlementFeatureGate, EntitlementResolver, ResolvedSet};
pub use features::{
    DefineFeature, DefineFeatureCommand, DeprecateFeature, ListFeatures, UpdateFeature,
    UpdateFeatureCommand,
};
pub use list_permissions::{ListPermissions, PermissionView};
pub use rebuild_tuples::RebuildAccessTuples;
pub use roles::{
    CreateCustomRole, CreateCustomRoleCommand, DeleteCustomRole, ListRoles, UpdateCustomRole,
    UpdateCustomRoleCommand,
};
pub use seed_features::SeedFeatures;
pub use seed_roles::SeedRoles;

/// Maps infrastructure failures into the domain vocabulary at the boundary.
pub(crate) fn storage(err: impl std::fmt::Display) -> crate::domain::AccessError {
    crate::domain::AccessError::Storage(format!("{err:#}"))
}
