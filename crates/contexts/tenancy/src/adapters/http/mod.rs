//! HTTP driving adapter: handlers, state and OpenAPI for the tenancy
//! context.

mod handlers;
mod routes;

pub use handlers::{
    __path_activate_tenant, __path_archive_tenant, __path_get_context, __path_get_settings,
    __path_get_tenant, __path_list_tenants, __path_provision_tenant, __path_public_tenant_context,
    __path_put_settings, __path_resume_tenant, __path_slug_availability, __path_suspend_tenant,
    TenancyState, activate_tenant, archive_tenant, get_context, get_settings, get_tenant,
    list_tenants, provision_tenant, public_tenant_context, put_settings, resume_tenant,
    slug_availability, state_for_tests, suspend_tenant,
};
pub use routes::{
    PERMISSIONED_ROUTES, platform_routes, public_routes, tenant_routes, tenant_scope_routes,
};

use utoipa::OpenApi;

/// OpenAPI document for this context; the API app nests it into the full
/// document served at `/openapi.json`.
#[derive(OpenApi)]
#[openapi(
    paths(
        handlers::provision_tenant,
        handlers::list_tenants,
        handlers::get_tenant,
        handlers::activate_tenant,
        handlers::suspend_tenant,
        handlers::resume_tenant,
        handlers::archive_tenant,
        handlers::get_context,
        handlers::get_settings,
        handlers::put_settings,
        handlers::slug_availability,
        handlers::public_tenant_context,
    ),
    components(schemas(
        crate::application::TenantView,
        crate::application::TenantListView,
        crate::application::ProvisionTenantCommand,
        crate::application::TenantSettingsOutput,
        crate::application::SettingsUpdate,
        crate::application::check_slug_availability::SlugAvailability,
        crate::application::tenant_lifecycle::SuspendTenantRequest,
        sanvi_http::TenantContext,
        sanvi_http::TenantRuntimeStatus,
        sanvi_http::ResolutionSource,
    )),
    tags(
        (name = "tenancy", description = "Tenants, lifecycle, settings and context resolution")
    )
)]
pub struct TenancyApiDoc;
