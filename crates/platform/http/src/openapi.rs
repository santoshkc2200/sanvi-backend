use utoipa::OpenApi;

use crate::error::{ApiError, ProblemDetail};

/// Schemas shared by every context's OpenAPI document.
///
/// Each context exports its own `#[derive(OpenApi)]` doc; the composition
/// root merges them and mounts the result at `/openapi.json` plus a Scalar UI
/// at `/scalar`.
#[derive(OpenApi)]
#[openapi(
    components(schemas(ProblemDetail, ApiError)),
    tags(
        (name = "system", description = "System, health and build information")
    )
)]
pub struct BaseApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_doc_serializes() {
        let json = BaseApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("\"openapi\""));
        assert!(json.contains("ProblemDetail"));
    }
}
