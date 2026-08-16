use sanvi_http::{ApiError, ProblemDetail};
use utoipa::OpenApi;

/// The merged OpenAPI document: every context contributes its paths and
/// schemas; the platform contributes the shared error model.
#[derive(OpenApi)]
#[openapi(
    paths(
        sanvi_system::adapters::http::healthz,
        sanvi_system::adapters::http::readyz,
        sanvi_system::adapters::http::version,
        sanvi_tenancy::adapters::http::provision_tenant,
        sanvi_tenancy::adapters::http::list_tenants,
        sanvi_tenancy::adapters::http::get_tenant,
        sanvi_tenancy::adapters::http::activate_tenant,
        sanvi_tenancy::adapters::http::suspend_tenant,
        sanvi_tenancy::adapters::http::resume_tenant,
        sanvi_tenancy::adapters::http::archive_tenant,
        sanvi_tenancy::adapters::http::get_context,
        sanvi_tenancy::adapters::http::get_settings,
        sanvi_tenancy::adapters::http::put_settings,
        sanvi_tenancy::adapters::http::slug_availability,
        sanvi_tenancy::adapters::http::public_tenant_context,
        sanvi_identity::adapters::http::get_me,
        sanvi_identity::adapters::http::list_sessions,
        sanvi_identity::adapters::http::revoke_session,
        sanvi_identity::adapters::http::start_link_challenge,
        sanvi_identity::adapters::http::complete_link_challenge,
        sanvi_identity::adapters::http::list_members,
        sanvi_identity::adapters::http::grant_member,
        sanvi_identity::adapters::http::update_member_roles,
        sanvi_identity::adapters::http::remove_member,
        sanvi_identity::adapters::http::list_invitations,
        sanvi_identity::adapters::http::invite_member,
        sanvi_identity::adapters::http::accept_invitation,
        sanvi_identity::adapters::http::resend_invitation,
        sanvi_identity::adapters::http::revoke_invitation,
        sanvi_access::adapters::http::list_permissions,
        sanvi_access::adapters::http::list_roles_platform,
        sanvi_access::adapters::http::list_roles_tenant,
        sanvi_access::adapters::http::create_role,
        sanvi_access::adapters::http::update_role,
        sanvi_access::adapters::http::delete_role,
        sanvi_access::adapters::http::list_features,
        sanvi_access::adapters::http::define_feature,
        sanvi_access::adapters::http::update_feature,
        sanvi_access::adapters::http::deprecate_feature,
        sanvi_access::adapters::http::list_tenant_entitlements,
        sanvi_audit::adapters::http::list_audit,
        sanvi_platform_admin::adapters::http::search_tenant_admin_views,
        sanvi_platform_admin::adapters::http::get_tenant_admin_view,
        sanvi_platform_admin::adapters::http::create_impersonation,
        sanvi_platform_admin::adapters::http::list_impersonations,
        sanvi_platform_admin::adapters::http::revoke_impersonation,
        sanvi_platform_admin::adapters::http::grant_entitlement_override,
        sanvi_platform_admin::adapters::http::list_entitlement_overrides,
        sanvi_platform_admin::adapters::http::revoke_entitlement_override,
        sanvi_platform_admin::adapters::http::list_pending_approvals,
        sanvi_platform_admin::adapters::http::approve_request,
        sanvi_platform_admin::adapters::http::reject_request,
    ),
    components(schemas(ProblemDetail, ApiError)),
    tags(
        (name = "system", description = "System, health and build information"),
        (name = "tenancy", description = "Tenants, lifecycle, settings and context resolution"),
        (name = "identity", description = "Users, memberships, invitations and account linking"),
        (name = "access", description = "Permissions, roles, features and entitlements"),
        (name = "audit", description = "Append-only, hash-chained audit trail"),
        (name = "platform_admin", description = "Operator control plane: tenants, impersonation, approvals")
    ),
    info(
        title = "Sanvi API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Multi-tenant SaaS platform API (v1)",
    )
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_contains_the_system_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/healthz"));
        assert!(json.contains("/readyz"));
        assert!(json.contains("/api/v1/system/version"));
        assert!(json.contains("ProblemDetail"));
    }

    #[test]
    fn doc_contains_the_tenancy_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/platform/tenants"));
        assert!(json.contains("/api/v1/tenant/context"));
        assert!(json.contains("/api/v1/tenant/settings"));
        assert!(json.contains("/api/v1/public/slug-availability"));
        assert!(json.contains("/api/v1/public/tenant-context"));
        assert!(json.contains("TenantView"));
        assert!(json.contains("TenantContext"));
    }

    #[test]
    fn doc_contains_the_identity_and_access_contracts() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/me"));
        assert!(json.contains("/api/v1/tenant/members"));
        assert!(json.contains("/api/v1/tenant/invitations"));
        assert!(json.contains("/api/v1/auth/link/challenge"));
        assert!(json.contains("/api/v1/access/permissions"));
        assert!(json.contains("/api/v1/tenant/roles"));
        assert!(json.contains("/api/v1/platform/features"));
        assert!(json.contains("/api/v1/tenant/entitlements"));
        assert!(json.contains("MeView"));
        assert!(json.contains("MemberView"));
        assert!(json.contains("PermissionView"));
        assert!(json.contains("RoleView"));
        assert!(json.contains("FeatureView"));
    }

    #[test]
    fn doc_contains_the_platform_admin_and_audit_contracts() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/platform/admin/tenants"));
        assert!(json.contains("/api/v1/platform/impersonations"));
        assert!(json.contains("/api/v1/platform/audit"));
        assert!(json.contains("/api/v1/platform/approvals"));
        assert!(json.contains("TenantAdminView"));
        assert!(json.contains("ImpersonationGrantView"));
        assert!(json.contains("AuditPageView"));
        assert!(json.contains("ApprovalView"));
    }
}
