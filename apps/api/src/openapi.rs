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
    ),
    components(schemas(ProblemDetail, ApiError)),
    tags(
        (name = "system", description = "System, health and build information"),
        (name = "tenancy", description = "Tenants, lifecycle, settings and context resolution")
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
}
