use actix_web::{HttpResponse, ResponseError, http::StatusCode};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Base URI for the stable `type` URIs of every problem class.
pub const PROBLEM_TYPE_BASE: &str = "https://sanvi.dev/problems";

/// RFC 9457 problem detail. This is the only error body the API produces.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProblemDetail {
    /// Stable URI identifying the error class.
    #[schema(example = "https://sanvi.dev/problems/bad-request")]
    pub r#type: String,
    /// Short, human-readable, stable title.
    pub title: String,
    /// HTTP status code (mirrors the response code).
    pub status: u16,
    /// Human-readable explanation of this specific occurrence.
    pub detail: String,
    /// URI of the offending resource, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Correlates with the request id and the trace in OpenObserve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Application-level API error.
///
/// Contexts map their domain errors into these at the adapter boundary; the
/// HTTP status lives in exactly one place (here), never scattered across
/// handlers.
#[derive(Debug, Clone, thiserror::Error, ToSchema)]
pub enum ApiError {
    #[error("{detail}")]
    BadRequest { detail: String },

    #[error("authentication required")]
    Unauthorized,

    #[error("{detail}")]
    Forbidden { detail: String },

    #[error("resource not found: {resource}")]
    NotFound { resource: String },

    #[error("{detail}")]
    Conflict { detail: String },

    #[error("rate limit exceeded")]
    TooManyRequests { retry_after_secs: u64 },

    #[error("request timed out")]
    RequestTimeout,

    #[error("{detail}")]
    ServiceUnavailable { detail: String },

    #[error("internal server error")]
    Internal { detail: String },
}

impl ApiError {
    /// Domain-error → HTTP status mapping. This is the single source of truth.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden { .. } => StatusCode::FORBIDDEN,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::Conflict { .. } => StatusCode::CONFLICT,
            Self::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::RequestTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::ServiceUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable `type` URI class slug.
    pub fn type_slug(&self) -> &'static str {
        match self {
            Self::BadRequest { .. } => "bad-request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden { .. } => "forbidden",
            Self::NotFound { .. } => "not-found",
            Self::Conflict { .. } => "conflict",
            Self::TooManyRequests { .. } => "too-many-requests",
            Self::RequestTimeout => "request-timeout",
            Self::ServiceUnavailable { .. } => "service-unavailable",
            Self::Internal { .. } => "internal-error",
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::BadRequest { .. } => "Bad Request",
            Self::Unauthorized => "Unauthorized",
            Self::Forbidden { .. } => "Forbidden",
            Self::NotFound { .. } => "Not Found",
            Self::Conflict { .. } => "Conflict",
            Self::TooManyRequests { .. } => "Too Many Requests",
            Self::RequestTimeout => "Gateway Timeout",
            Self::ServiceUnavailable { .. } => "Service Unavailable",
            Self::Internal { .. } => "Internal Server Error",
        }
    }

    pub fn problem(&self) -> ProblemDetail {
        ProblemDetail {
            r#type: format!("{PROBLEM_TYPE_BASE}/{}", self.type_slug()),
            title: self.title().to_string(),
            status: self.status().as_u16(),
            detail: self.to_string(),
            instance: None,
            trace_id: None,
        }
    }

    pub fn with_context(self, instance: Option<String>, trace_id: Option<String>) -> ProblemDetail {
        let mut problem = self.problem();
        problem.instance = instance;
        problem.trace_id = trace_id;
        problem
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    pub fn service_unavailable(detail: impl Into<String>) -> Self {
        Self::ServiceUnavailable {
            detail: detail.into(),
        }
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::BadRequest {
            detail: detail.into(),
        }
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status()
    }

    fn error_response(&self) -> HttpResponse {
        let mut response = HttpResponse::build(self.status()).json(self.problem());
        if let Self::TooManyRequests { retry_after_secs } = self {
            response.headers_mut().insert(
                actix_web::http::header::RETRY_AFTER,
                retry_after_secs.to_string().parse().unwrap(),
            );
        }
        response
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal {
            detail: format!("{err:#}"),
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(err: serde_json::Error) -> Self {
        Self::BadRequest {
            detail: err.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// ErrorHandlers layer: every 4xx/5xx response leaves the API as RFC 9457
// problem+json with `instance` and `trace_id` populated from the request.
// ---------------------------------------------------------------------------

use actix_web::{
    HttpMessage,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
};

use crate::middleware::RequestId;

/// Mountable error handler enriching every error response. Sits innermost in
/// the middleware chain (wrapping routes), where the body type is `BoxBody`.
#[derive(Clone)]
pub struct ErrorHandlersLayer;

impl<S> actix_web::dev::Transform<S, ServiceRequest> for ErrorHandlersLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = ErrorHandlersLayerService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(ErrorHandlersLayerService { service }))
    }
}

pub struct ErrorHandlersLayerService<S> {
    service: S,
}

impl<S> actix_web::dev::Service<ServiceRequest> for ErrorHandlersLayerService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let fut = self.service.call(req);
        Box::pin(async move {
            let res = fut.await?;
            Ok(enrich(res).await)
        })
    }
}

/// Normalize one response into problem+json when it is an error.
async fn enrich(res: ServiceResponse<BoxBody>) -> ServiceResponse<BoxBody> {
    let status = res.status();
    if !status.is_client_error() && !status.is_server_error() {
        return res;
    }

    let instance = res.request().path().to_string();
    let trace_id = res
        .request()
        .extensions()
        .get::<RequestId>()
        .map(|id| id.to_string());

    let (req, response) = res.into_parts();
    let (_, body) = response.into_parts();

    // If the body is already problem+json, enrich the correlation fields;
    // otherwise replace it with a canonical problem body.
    let problem = {
        let body_bytes = actix_web::body::to_bytes(body).await.unwrap_or_default();
        let existing: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
        let mut problem: ProblemDetail = serde_json::from_value(existing)
            .unwrap_or_else(|_| fallback_problem(status, &instance, trace_id.clone()));
        problem.instance = Some(instance);
        problem.trace_id = trace_id;
        problem
    };

    let bytes = serde_json::to_vec(&problem).unwrap_or_default();
    let body = BoxBody::new(actix_http::body::SizedStream::new(
        bytes.len() as u64,
        futures_util::stream::once(async move {
            Ok::<_, std::io::Error>(actix_web::web::Bytes::from(bytes))
        }),
    ));

    let mut new_response = actix_web::HttpResponse::build(status).body(body);
    new_response.headers_mut().insert(
        actix_web::http::header::CONTENT_TYPE,
        actix_web::http::header::HeaderValue::from_static("application/problem+json"),
    );

    ServiceResponse::new(req, new_response)
}

fn fallback_problem(
    status: actix_web::http::StatusCode,
    instance: &str,
    trace_id: Option<String>,
) -> ProblemDetail {
    ProblemDetail {
        r#type: format!("{PROBLEM_TYPE_BASE}/error"),
        title: status.canonical_reason().unwrap_or("Error").to_string(),
        status: status.as_u16(),
        detail: String::new(),
        instance: Some(instance.to_string()),
        trace_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_is_consistent() {
        assert_eq!(ApiError::bad_request("x").status(), StatusCode::BAD_REQUEST);
        assert_eq!(ApiError::Unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            ApiError::NotFound {
                resource: "x".into()
            }
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::service_unavailable("x").status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ApiError::RequestTimeout.status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            ApiError::TooManyRequests {
                retry_after_secs: 1
            }
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn problem_body_is_rfc9457_shaped() {
        let problem = ApiError::bad_request("field x is required").with_context(
            Some("/api/v1/system".to_string()),
            Some("req-123".to_string()),
        );
        assert_eq!(problem.status, 400);
        assert_eq!(problem.r#type, "https://sanvi.dev/problems/bad-request");
        assert_eq!(problem.title, "Bad Request");
        assert_eq!(problem.instance.as_deref(), Some("/api/v1/system"));
        assert_eq!(problem.trace_id.as_deref(), Some("req-123"));
    }

    #[test]
    fn error_response_carries_retry_after_for_rate_limits() {
        let response = ApiError::TooManyRequests {
            retry_after_secs: 2,
        }
        .error_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "2");
    }

    #[tokio::test]
    async fn problem_json_has_all_required_fields() {
        let body = ApiError::Unauthorized.error_response();
        let bytes = actix_web::body::to_bytes(body.into_body()).await.unwrap();
        let text: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(text.get("type").is_some());
        assert!(text.get("title").is_some());
        assert!(text.get("status").is_some());
        assert!(text.get("detail").is_some());
    }
}
