//! The two cross-cutting trace conventions (TASK-020 step 1), fixed once
//! here rather than per context:
//!
//! 1. Every problem-details error carries a `trace_id` equal to the W3C
//!    trace id of the span that was active when the error was rendered, so
//!    the body joins the trace in OpenObserve without a lookup. With no
//!    active OpenTelemetry context (local runs with OTLP off) it falls back
//!    to the `x-request-id` value the error layer already sets —
//!    correlation degrades, it never disappears.
//! 2. Every response carries a W3C `traceparent` header referencing *this
//!    server's* span, so an SSR document or an API client can link its own
//!    telemetry to ours. The header is set only when a valid span context
//!    exists; a response without one means the request was served outside
//!    any span, not that tracing is broken.
//!
//! Inbound `traceparent` headers join the caller's trace at the root span
//! (`SanviRootSpanBuilder` extracts through the global propagator, which
//! `sanvi_telemetry::init` installs); the response header here closes the
//! loop in the other direction.

use opentelemetry::trace::TraceContextExt as _;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// The W3C trace id (32 lowercase hex chars) of the active span, when that
/// span carries a valid OpenTelemetry context.
pub fn current_trace_id() -> Option<String> {
    let context = Span::current().context();
    let otel_span = context.span();
    let span_context = otel_span.span_context();
    if !span_context.is_valid() {
        return None;
    }
    Some(hex(&span_context.trace_id().to_bytes()))
}

/// The W3C `traceparent` value (`00-<trace-id>-<span-id>-<flags>`) of the
/// active span, when that span carries a valid OpenTelemetry context.
pub fn current_traceparent() -> Option<String> {
    let context = Span::current().context();
    let otel_span = context.span();
    let span_context = otel_span.span_context();
    if !span_context.is_valid() {
        return None;
    }
    let flags = if span_context.trace_flags().is_sampled() {
        "01"
    } else {
        "00"
    };
    Some(format!(
        "00-{}-{}-{flags}",
        hex(&span_context.trace_id().to_bytes()),
        hex(&span_context.span_id().to_bytes()),
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const TRACEPARENT_HEADER: &str = "traceparent";

/// Middleware stamping the W3C `traceparent` response header on every
/// response. Innermost to the request span (it sits inside
/// `TracingLogger`), so `Span::current()` is the request span — the same
/// span whose id the error layer puts into `ProblemDetail.trace_id`.
#[derive(Clone, Default)]
pub struct TraceparentLayer;

impl<S, B> actix_web::dev::Transform<S, actix_web::dev::ServiceRequest> for TraceparentLayer
where
    S: actix_web::dev::Service<
            actix_web::dev::ServiceRequest,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        > + 'static,
    B: actix_web::body::MessageBody + 'static,
{
    type Response = actix_web::dev::ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = TraceparentLayerService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(TraceparentLayerService { service }))
    }
}

pub struct TraceparentLayerService<S> {
    service: S,
}

impl<S, B> actix_web::dev::Service<actix_web::dev::ServiceRequest> for TraceparentLayerService<S>
where
    S: actix_web::dev::Service<
            actix_web::dev::ServiceRequest,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        > + 'static,
    B: actix_web::body::MessageBody + 'static,
{
    type Response = actix_web::dev::ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: actix_web::dev::ServiceRequest) -> Self::Future {
        let fut = self.service.call(req);
        Box::pin(async move {
            let mut res = fut.await?;
            if let Some(traceparent) = current_traceparent()
                && let Ok(value) = actix_web::http::header::HeaderValue::from_str(&traceparent)
            {
                res.headers_mut().insert(
                    actix_web::http::header::HeaderName::from_static(TRACEPARENT_HEADER),
                    value,
                );
            }
            Ok(res)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, HttpResponse, web};
    use opentelemetry::trace::TracerProvider as _;
    use std::sync::Mutex;

    /// A subscriber whose OpenTelemetry layer binds a *real* SDK tracer, so
    /// spans carry valid trace ids — the shape a production boot has, which
    /// the noop fallback paths deliberately degrade from.
    fn otel_guard() -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::layer::SubscriberExt as _;
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("trace-context-test")),
        );
        tracing::subscriber::set_default(subscriber)
    }

    /// The trace id in an error body equals the id of the span that was
    /// active when the error was rendered — the DoD assertion, checked
    /// against a real SDK tracer rather than the fallback.
    #[actix_web::test]
    async fn error_trace_id_equals_the_active_span_trace_id() {
        use tracing::Instrument as _;

        let _otel = otel_guard();
        let seen: web::Data<Mutex<Option<(String, Option<String>)>>> =
            web::Data::new(Mutex::new(None));

        let app = actix_web::test::init_service(
            App::new()
                .app_data(seen.clone())
                // ErrorHandlers innermost (it rebuilds error bodies and
                // drops headers added inside it) — the same relative order
                // the assembled stack uses.
                .wrap(crate::error::ErrorHandlersLayer)
                .wrap(TraceparentLayer)
                .service(web::resource("/boom").route(web::get().to(
                    |seen: web::Data<Mutex<Option<(String, Option<String>)>>>| async move {
                        *seen.lock().unwrap() =
                            Some((current_trace_id().unwrap(), current_traceparent()));
                        HttpResponse::InternalServerError().finish()
                    },
                ))),
        )
        .await;

        // The span under test stands in for the request span
        // `TracingLogger` creates in the assembled app.
        let span = tracing::info_span!("test request");

        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get().uri("/boom").to_request(),
        )
        .instrument(span.clone())
        .await;
        assert_eq!(response.status().as_u16(), 500);

        let body = actix_web::test::read_body(response).await;
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let (span_trace_id, _) = seen.lock().unwrap().clone().expect("handler saw a span");

        // The body names the same trace as the span that served the request…
        assert_eq!(problem["trace_id"], serde_json::json!(span_trace_id));
        assert_eq!(span_trace_id.len(), 32);
        assert!(span_trace_id.chars().all(|c| c.is_ascii_hexdigit()));

        // …and the response headers carry a traceparent for the same trace.
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get().uri("/boom").to_request(),
        )
        .instrument(span)
        .await;
        let header_traceparent = response
            .headers()
            .get(TRACEPARENT_HEADER)
            .map(|value| value.to_str().unwrap().to_string());
        assert_eq!(
            header_traceparent.as_deref(),
            seen.lock().unwrap().as_ref().and_then(|(_, tp)| tp.as_deref()),
            "header traceparent must reference the request span"
        );
        let traceparent = header_traceparent.expect("traceparent header on responses");
        let parts: Vec<&str> = traceparent.split('-').collect();
        assert_eq!(parts[0], "00");
        let stored = seen.lock().unwrap().clone().unwrap();
        assert_eq!(parts[1], stored.0);
        assert_eq!(parts[2].len(), 16);
    }

    /// Without an OpenTelemetry layer there is no valid span context: the
    /// conventions degrade (request-id fallback, no traceparent header)
    /// instead of failing the request.
    #[actix_web::test]
    async fn without_otel_the_conventions_degrade_instead_of_failing() {
        let app = actix_web::test::init_service(
            App::new()
                .wrap(TraceparentLayer)
                .service(
                    web::resource("/boom")
                        .route(web::get().to(|| async { HttpResponse::InternalServerError().finish() })),
                ),
        )
        .await;
        let response = actix_web::test::call_service(&app, actix_web::test::TestRequest::get().uri("/boom").to_request())
            .await;
        assert_eq!(response.status().as_u16(), 500);
        assert!(response.headers().get(TRACEPARENT_HEADER).is_none());

        assert!(current_trace_id().is_none(), "a noop context must not fabricate ids");
        assert!(current_traceparent().is_none());
    }

    #[test]
    fn hex_encodes_bytes_without_padding_gaps() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
