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
    /// Correlates with the trace in OpenObserve: the W3C trace id of the
    /// span serving the request (the same trace the `traceparent` response
    /// header references), falling back to the request id when the request
    /// ran outside any OpenTelemetry context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Machine-readable reason for statuses that carry one (e.g. tenant
    /// suspension); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// RFC 9457 extension members for problem classes that carry
    /// structured extras (the disconnect conflict names its blockers,
    /// for example). Flattened onto the JSON object exactly as the RFC
    /// prescribes; carrying them here keeps the error-enrichment layer
    /// from stripping them. Absent otherwise.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub extensions: Option<std::collections::BTreeMap<String, serde_json::Value>>,
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

    /// A conflict carrying a machine-readable `reason`, surfaced as
    /// `ProblemDetail.reason`, so a client can distinguish conflicts of the
    /// same status without parsing `detail` — cap version races, for
    /// example, where missing-precondition and lost-race need different
    /// recovery. The status and `type` stay the plain conflict class.
    #[error("{detail}")]
    ConflictReasoned { detail: String, reason: String },

    #[error("resource is gone: {detail}")]
    Gone { detail: String },

    #[error("upstream service rejected the request: {detail}")]
    BadGateway { detail: String },

    #[error("tenant is suspended")]
    TenantSuspended,

    #[error("rate limit exceeded")]
    TooManyRequests { retry_after_secs: u64 },

    #[error("request timed out")]
    RequestTimeout,

    /// Load shedding (TASK-023, NFR-1106): the platform is too busy. This is
    /// deliberately a distinct class from [`ApiError::ServiceUnavailable`]
    /// even though both are `503`: `429` means the *caller* asked too often
    /// (TASK-026), `platform/overloaded` means the *platform* is too busy —
    /// a client backs off exponentially for the first and retries after
    /// `Retry-After` for the second. Conflating them makes client backoff
    /// wrong in both directions.
    #[error("platform is overloaded, retry later")]
    Overloaded { retry_after_secs: u64 },

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
            Self::Conflict { .. } | Self::ConflictReasoned { .. } => StatusCode::CONFLICT,
            Self::Gone { .. } => StatusCode::GONE,
            Self::BadGateway { .. } => StatusCode::BAD_GATEWAY,
            Self::TenantSuspended => StatusCode::LOCKED,
            Self::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::RequestTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Overloaded { .. } | Self::ServiceUnavailable { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
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
            Self::Conflict { .. } | Self::ConflictReasoned { .. } => "conflict",
            Self::Gone { .. } => "gone",
            Self::BadGateway { .. } => "bad-gateway",
            Self::TenantSuspended => "tenant-suspended",
            Self::TooManyRequests { .. } => "too-many-requests",
            Self::RequestTimeout => "request-timeout",
            Self::Overloaded { .. } => "platform/overloaded",
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
            Self::Conflict { .. } | Self::ConflictReasoned { .. } => "Conflict",
            Self::Gone { .. } => "Gone",
            Self::BadGateway { .. } => "Bad Gateway",
            Self::TenantSuspended => "Tenant Suspended",
            Self::TooManyRequests { .. } => "Too Many Requests",
            Self::RequestTimeout => "Gateway Timeout",
            Self::Overloaded { .. } => "Platform Overloaded",
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
            reason: match self {
                Self::ConflictReasoned { reason, .. } => Some(reason.clone()),
                _ => None,
            },
            extensions: None,
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

    /// Map a storage-layer failure rendered by an adapter into the contract's
    /// two shapes (TASK-021, FR-1110). Statement timeouts — Postgres cancels
    /// the query with `canceling statement due to statement timeout` once
    /// `database.statement_timeout_ms` elapses — surface as the same
    /// `request-timeout` problem the request-budget middleware emits, so a
    /// request that cannot finish inside its budget gets a problem-details
    /// response instead of a held connection and an eventual 500. Everything
    /// else stays the opaque internal class: infrastructure detail is for the
    /// logs, not the client.
    pub fn from_storage_detail(detail: &str) -> Self {
        if detail.contains("canceling statement due to statement timeout") {
            Self::RequestTimeout
        } else {
            Self::internal("storage failure")
        }
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status()
    }

    fn error_response(&self) -> HttpResponse {
        let mut response = HttpResponse::build(self.status()).json(self.problem());
        if let Self::TooManyRequests { retry_after_secs } | Self::Overloaded { retry_after_secs } =
            self
        {
            response.headers_mut().insert(
                actix_web::http::header::RETRY_AFTER,
                retry_after_secs.to_string().parse().unwrap(),
            );
        }
        response
    }
}

/// Port for localizing problem details (phase 06). Implemented by the
/// localization context; absent when localization is not wired, in which
/// case problem bodies stay in the English defaults.
pub trait ProblemTranslator: Send + Sync {
    /// Translate the stable title of a problem class. `type_slug` is the
    /// path segment of the `type` URI (e.g. `bad-request`, `tenant-suspended`,
    /// `error` for the fallback). Returns `None` when no translation is
    /// known — callers keep the English default.
    fn title(&self, type_slug: &str, scope: &str, locale: &str) -> Option<String>;

    /// Translate the human-facing explanation for a problem class.
    fn detail(&self, type_slug: &str, scope: &str, locale: &str) -> Option<String>;
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
    web,
};

use crate::locale::LocaleContext;
use crate::middleware::RequestId;

/// App-data carrier for the optional [`ProblemTranslator`]. Registered by
/// the composition root when the localization context is wired.
pub struct ProblemTranslatorData(pub std::sync::Arc<dyn ProblemTranslator>);

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
    // TASK-020 convention: the body's `trace_id` names the trace, not just
    // the request — the active span's W3C trace id, with the request id kept
    // as the fallback for spans outside any OpenTelemetry context.
    let trace_id = crate::trace_context::current_trace_id().or_else(|| {
        res.request()
            .extensions()
            .get::<RequestId>()
            .map(|id| id.to_string())
    });

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

    // Phase 06: localize human-facing fields. `type`/`status`/`reason` stay
    // machine-readable and stable.
    let problem = localize_problem(req.extensions().get::<LocaleContext>(), &req, problem);

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
        reason: None,
        extensions: None,
    }
}

/// Replace the title of `problem` with a localized one when the request has
/// a negotiated locale and a [`ProblemTranslator`] is registered.
fn localize_problem(
    locale: Option<&LocaleContext>,
    req: &actix_web::HttpRequest,
    mut problem: ProblemDetail,
) -> ProblemDetail {
    let Some(context) = locale else {
        return problem;
    };
    let Some(translator) = req.app_data::<web::Data<ProblemTranslatorData>>() else {
        return problem;
    };
    let type_slug = problem
        .r#type
        .strip_prefix(PROBLEM_TYPE_BASE)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or("error");
    let scope = context
        .tenant_id
        .map(|tenant_id| tenant_id.to_string())
        .unwrap_or_else(|| "platform".to_string());
    if let Some(title) = translator.0.title(type_slug, &scope, &context.locale) {
        problem.title = title;
    }
    if let Some(detail) = translator.0.detail(type_slug, &scope, &context.locale) {
        problem.detail = detail;
    }
    problem
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

    /// TASK-023 (NFR-1106): the overload class is a distinct problem type
    /// from the dependency-down class, and it is the only variant that
    /// claims that meaning. Both ride `503`, but the `type` URI tells the
    /// client which backoff applies.
    #[test]
    fn overloaded_is_its_own_problem_type_with_retry_after() {
        let response = ApiError::Overloaded {
            retry_after_secs: 7,
        }
        .error_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get("retry-after").unwrap(), "7");

        let problem = ApiError::Overloaded {
            retry_after_secs: 7,
        }
        .problem();
        assert_eq!(
            problem.r#type,
            "https://sanvi.dev/problems/platform/overloaded"
        );
        assert_eq!(problem.title, "Platform Overloaded");
        assert_eq!(problem.status, 503);

        // Nothing else claims the overloaded meaning: the dependency-down
        // class keeps its own slug on the same status.
        assert_eq!(
            ApiError::service_unavailable("stripe unreachable").type_slug(),
            "service-unavailable"
        );
    }

    #[test]
    fn statement_timeouts_map_to_the_request_timeout_problem() {
        // The exact message Postgres raises when statement_timeout cancels a
        // query; pinned so a wording change upstream shows up here first.
        let cancelled =
            "error returned from database: canceling statement due to statement timeout";
        let error = ApiError::from_storage_detail(cancelled);
        assert_eq!(error.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(error.type_slug(), "request-timeout");

        // Lock timeouts share SQLSTATE 57014 but a different budget; they are
        // not this contract's class.
        let lock = "error returned from database: canceling statement due to lock timeout";
        assert_eq!(
            ApiError::from_storage_detail(lock).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );

        let other = "error returned from database: relation does not exist";
        assert_eq!(
            ApiError::from_storage_detail(other).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
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

    struct FakeTranslator;

    impl ProblemTranslator for FakeTranslator {
        fn title(&self, type_slug: &str, _scope: &str, locale: &str) -> Option<String> {
            match (type_slug, locale) {
                ("bad-request", "ja") => Some("不正なリクエスト".to_string()),
                _ => None,
            }
        }

        fn detail(&self, type_slug: &str, scope: &str, locale: &str) -> Option<String> {
            match (type_slug, locale) {
                ("bad-request", "ja") => {
                    Some(format!("{scope}: リクエストを処理できませんでした。"))
                }
                _ => None,
            }
        }
    }

    #[test]
    fn localize_title_translates_and_keeps_machine_readable_fields() {
        let mut problem = ApiError::bad_request("x").problem();
        assert_eq!(problem.title, "Bad Request");

        let translated = FakeTranslator
            .title("bad-request", "platform", "ja")
            .unwrap();
        problem.title = translated;
        assert_eq!(problem.title, "不正なリクエスト");
        // The stable fields are untouched by localization.
        assert_eq!(problem.r#type, "https://sanvi.dev/problems/bad-request");
        assert_eq!(problem.status, 400);
    }

    #[test]
    fn localize_problem_uses_tenant_scope_for_title_and_detail() {
        let tenant_id = sanvi_kernel::TenantId::try_from(1_000_001_i64).unwrap();
        let translator: std::sync::Arc<dyn ProblemTranslator> = std::sync::Arc::new(FakeTranslator);
        let req = actix_web::test::TestRequest::default()
            .app_data(web::Data::new(ProblemTranslatorData(translator)))
            .to_http_request();
        let context = LocaleContext {
            locale: "ja".to_string(),
            source: crate::locale::LocaleSource::TenantDefault,
            fallback_chain: vec!["ja".to_string(), "en".to_string()],
            timezone: "Asia/Tokyo".to_string(),
            tenant_id: Some(tenant_id),
        };
        let problem =
            localize_problem(Some(&context), &req, ApiError::bad_request("raw").problem());
        assert_eq!(problem.title, "不正なリクエスト");
        assert_eq!(
            problem.detail,
            format!("{tenant_id}: リクエストを処理できませんでした。")
        );
        assert_eq!(problem.r#type, "https://sanvi.dev/problems/bad-request");
        assert_eq!(problem.status, 400);
    }
}
