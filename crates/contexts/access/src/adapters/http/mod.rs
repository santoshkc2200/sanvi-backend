//! HTTP driving adapter: handlers + routes.

pub mod catalog;
mod handlers;
pub mod routes;

pub use handlers::{
    __path_create_role, __path_define_feature, __path_delete_role, __path_deprecate_feature,
    __path_list_features, __path_list_permissions, __path_list_roles_platform,
    __path_list_roles_tenant, __path_list_tenant_entitlements, __path_update_feature,
    __path_update_role, AccessState, FeatureView, ResolvedEntitlementView, RoleView, create_role,
    define_feature, delete_role, deprecate_feature, list_features, list_permissions,
    list_roles_platform, list_roles_tenant, update_feature, update_role,
};
pub use routes::{access_routes, tenant_scope_routes};
