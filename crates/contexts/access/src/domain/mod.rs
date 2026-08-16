//! Domain: pure vocabulary for the access context.

pub mod error;
pub mod permissions;
pub mod ports;
pub mod roles;

pub use error::AccessError;
pub use permissions::{PERMISSIONS, Permission, PermissionDef, PermissionScope, lookup};
pub use ports::{
    Authorizer, Clock, Decision, DecisionCache, IdGenerator, PermissionStore, ResourceRef,
    RoleRepository, Tuple,
};
pub use roles::{Role, RoleId, RoleScope, SYSTEM_ROLE_SEEDS};
