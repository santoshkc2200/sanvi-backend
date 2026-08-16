//! Application: use cases for the access context.

pub mod check_permission;
pub mod list_permissions;
pub mod rebuild_tuples;
pub mod seed_roles;

pub use check_permission::{CachedAuthorizer, CheckPermission};
pub use list_permissions::{ListPermissions, PermissionView};
pub use rebuild_tuples::RebuildAccessTuples;
pub use seed_roles::SeedRoles;

/// Maps infrastructure failures into the domain vocabulary at the boundary.
pub(crate) fn storage(err: impl std::fmt::Display) -> crate::domain::AccessError {
    crate::domain::AccessError::Storage(err.to_string())
}
