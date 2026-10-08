//! The trace conventions against the *assembled* middleware stack.
//!
//! `trace_context.rs`'s unit tests wrap `TraceparentLayer` by hand, which
//! cannot catch an ordering regression in `middleware::stack`: the layer
//! only sees the request span (`TracingLogger`) while it sits *inside* it,
//! because `tracing-actix-web` holds the span entered only across the
//! inner future's polls. This file pins the real wrap order — a regression
//! to the wrong side of the span leaves the header silently absent.

use std::sync::Mutex;

use actix_web::{App, HttpResponse, web};
use opentelemetry::trace::TracerProvider as _;
use sanvi_http::{ServerConfig, middleware, trace_context};
use tracing_subscriber::layer::SubscriberExt as _;

/// A subscriber whose OpenTelemetry layer binds a *real* SDK tracer, so the
/// request span carries a valid trace id — the shape a production boot has.
fn otel_guard() -> tracing::subscriber::DefaultGuard {
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer().with_tracer(provider.tracer("trace-context-integration")),
    );
    tracing::subscriber::set_default(subscriber)
}

async fn probe(seen: web::Data<Mutex<Option<String>>>) -> HttpResponse {
    *seen.lock().unwrap() = trace_context::current_trace_id();
    HttpResponse::Ok().finish()
}

/// Through the full `middleware::stack`, the response carries a
/// `traceparent` header naming the same trace the handler ran in — the
/// pairing the TASK-020 conventions promise SSR clients and API consumers.
#[actix_web::test]
async fn the_assembled_stack_stamps_the_traceparent_header() {
    let _otel = otel_guard();
    let seen: web::Data<Mutex<Option<String>>> = web::Data::new(Mutex::new(None));
    let app = actix_web::test::init_service(middleware::stack(
        App::new()
            .app_data(seen.clone())
            .route("/probe", web::get().to(probe)),
        ServerConfig::default(),
        None,
        None,
        None,
        None,
        None,
        sanvi_http::ssr_cache::SsrCacheLayer::disabled(
            sanvi_http::config::SsrCacheConfig::default(),
        ),
        sanvi_http::degraded::DegradedHeaderLayer::disabled(),
        None,
    ))
    .await;

    let response = actix_web::test::call_service(
        &app,
        actix_web::test::TestRequest::get()
            .uri("/probe")
            .to_request(),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);

    let handler_trace_id = seen
        .lock()
        .unwrap()
        .clone()
        .expect("handler ran inside the request span");
    let header = response
        .headers()
        .get("traceparent")
        .map(|v| v.to_str().unwrap().to_string())
        .expect("traceparent header stamped by the assembled stack");
    assert_eq!(header.split('-').nth(1), Some(handler_trace_id.as_str()));
    assert_eq!(header.split('-').next(), Some("00"));
}
