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
    ),
    components(schemas(ProblemDetail, ApiError)),
    tags(
        (name = "system", description = "System, health and build information")
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
}
