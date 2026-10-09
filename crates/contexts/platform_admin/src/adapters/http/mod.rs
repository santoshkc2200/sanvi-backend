//! HTTP driving adapter: handlers + routes.

pub mod catalog;
mod handlers;
pub mod routes;
pub mod validation;

pub use handlers::{
    __path_approve_request, __path_create_impersonation, __path_disable_application_fee,
    __path_get_application_fee, __path_get_tenant_admin_view, __path_grant_entitlement_override,
    __path_list_entitlement_overrides, __path_list_impersonations, __path_list_pending_approvals,
    __path_reject_request, __path_revoke_entitlement_override, __path_revoke_impersonation,
    __path_search_tenant_admin_views, __path_set_application_fee, PlatformAdminState,
    approve_request, create_impersonation, disable_application_fee, get_application_fee,
    get_tenant_admin_view, grant_entitlement_override, list_entitlement_overrides,
    list_impersonations, list_pending_approvals, reject_request, revoke_entitlement_override,
    revoke_impersonation, search_tenant_admin_views, set_application_fee,
};
pub use routes::platform_admin_routes;
