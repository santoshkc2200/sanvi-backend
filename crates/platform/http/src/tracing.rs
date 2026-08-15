use std::time::Instant;

use actix_web::{
    HttpMessage,
    dev::{ServiceRequest, ServiceResponse},
    http::StatusCode,
};
use tracing::{Span, info_span};
use tracing_actix_web::RootSpanBuilder;

use crate::{metrics::context_from_path, middleware::RequestId};

/// Root span builder wiring request correlation into every request span.
///
/// Each span carries `request_id`, `http.*` and `sanvi.*` fields plus the
/// response status and duration. RED metrics are recorded by the dedicated
/// metrics middleware, which also observes rejections that never reach a
/// handler.
#[derive(Clone, Default)]
pub struct SanviRootSpanBuilder;

/// tracing 0.1 spans don't expose extensions; this map carries the start
/// instant for each active request span so `on_request_end` can compute
/// wall-clock duration.
static SPAN_STARTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<tracing::span::Id, Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

impl RootSpanBuilder for SanviRootSpanBuilder {
    fn on_request_start(request: &ServiceRequest) -> Span {
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .copied()
            .map(|id| id.to_string())
            .unwrap_or_default();

        let span = info_span!(
            "HTTP request",
            request_id = %request_id,
            http.method = %request.method(),
            http.target = %request.path(),
            http.host = %request.connection_info().host(),
            http.client_ip = %request.connection_info().realip_remote_addr().unwrap_or_default(),
            sanvi.context = context_from_path(request.path()),
        );
        if let (Some(id), Ok(mut starts)) = (span.id(), SPAN_STARTS.lock()) {
            starts.insert(id, Instant::now());
        }
        span
    }

    fn on_request_end<B: actix_web::body::MessageBody>(
        span: Span,
        outcome: &std::result::Result<ServiceResponse<B>, actix_web::Error>,
    ) {
        let duration = span
            .id()
            .and_then(|id| {
                SPAN_STARTS
                    .lock()
                    .ok()
                    .and_then(|mut starts| starts.remove(&id))
            })
            .map(|started| started.elapsed());

        let status = match outcome {
            Ok(response) => response.status().as_u16(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
        };

        span.record("http.status_code", status);
        if let Some(duration) = duration {
            span.record("sanvi.duration_ms", duration.as_millis() as u64);
        }
    }
}

/// Re-export for middleware.rs.
pub use tracing_actix_web::TracingLogger;
