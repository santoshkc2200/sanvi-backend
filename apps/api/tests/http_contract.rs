//! Contract tests: exercise the assembled `App` through HTTP.
//!
//! No live infrastructure: readiness probes are fakes (one healthy, one
//! degradable), so these tests cover the middleware stack, problem+json
//! shape, headers, rate limiting (disabled) and the OpenAPI endpoint.

use actix_web::{http::StatusCode, test};
use sanvi_api::{AppConfig, build_app, build_system_state};
use sanvi_system::domain::BuildInfo;
use sanvi_telemetry::MetricsRegistry;

fn test_config() -> AppConfig {
    let mut config = AppConfig::default();
    config.telemetry.service_name = "sanvi-api-test".to_string();
    config.server.rate_limit.enabled = false;
    config
}

fn test_app() -> actix_web::App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse<
            tracing_actix_web::StreamSpan<actix_web::body::BoxBody>,
        >,
        Error = actix_web::Error,
        InitError = (),
    > + 'static,
> {
    let config = test_config();
    let metrics = MetricsRegistry::prometheus_only();
    let build_info = BuildInfo::new(
        "0.1.0-test".into(),
        "test".into(),
        "now".into(),
        "test".into(),
        "sanvi-api-test".into(),
    );
    let system = build_system_state(build_info, pool_stub(), None, 1);
    build_app(config, metrics, system, None)
}

/// A lazy pool that never connects: the readiness probe is not part of this
/// test suite (covered by the context's own tests).
fn pool_stub() -> sanvi_persistence::PgPool {
    sanvi_persistence::PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .expect("lazy pool construction never connects")
}

#[actix_web::test]
async fn healthz_is_live() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get().uri("/healthz").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["status"], "ok");
}

#[actix_web::test]
async fn readyz_degrades_when_a_dependency_is_down() {
    // The stub pool cannot connect: readiness must answer 503 problem+json.
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get().uri("/readyz").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["status"], 503);
    assert!(
        body["type"]
            .as_str()
            .unwrap()
            .contains("service-unavailable")
    );
    assert!(body.get("instance").is_some());
}

#[actix_web::test]
async fn version_reports_build_info() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/system/version")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["version"], "0.1.0-test");
    assert_eq!(body["service_name"], "sanvi-api-test");
}

#[actix_web::test]
async fn unknown_route_is_a_404_problem() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get().uri("/api/v1/nope").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert!(body.get("type").is_some());
    assert!(body.get("status").is_some());
}

#[actix_web::test]
async fn responses_carry_security_headers_and_request_id() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get().uri("/healthz").to_request();
    let res = test::call_service(&app, req).await;

    assert_eq!(
        res.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(res.headers().get("x-frame-options").unwrap(), "DENY");
    assert_eq!(res.headers().get("referrer-policy").unwrap(), "same-origin");
    assert!(res.headers().contains_key("content-security-policy"));
    assert!(res.headers().contains_key("permissions-policy"));

    let request_id = res.headers().get("x-request-id").unwrap().to_str().unwrap();
    uuid::Uuid::parse_str(request_id).expect("request id must be a UUID");
}

#[actix_web::test]
async fn inbound_request_id_is_propagated() {
    let app = test::init_service(test_app()).await;
    let inbound = uuid::Uuid::now_v7();
    let req = test::TestRequest::get()
        .uri("/healthz")
        .insert_header(("x-request-id", inbound.to_string()))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(
        res.headers().get("x-request-id").unwrap().to_str().unwrap(),
        inbound.to_string()
    );
}

#[actix_web::test]
async fn openapi_json_is_served() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get().uri("/openapi.json").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["info"]["title"], "Sanvi API");
    assert!(body["paths"].get("/healthz").is_some());
}

#[actix_web::test]
async fn metrics_endpoint_renders_prometheus_format() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get().uri("/metrics").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap().to_str().unwrap(),
        "text/plain; version=0.0.4"
    );
}

#[actix_web::test]
async fn cors_preflight_from_allowed_origin_succeeds() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::with_uri("/healthz")
        .method(actix_web::http::Method::OPTIONS)
        .insert_header(("Origin", "http://localhost:5173"))
        .insert_header(("Access-Control-Request-Method", "GET"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key("access-control-allow-origin"));
}

#[actix_web::test]
async fn body_of_unknown_route_has_boxed_type_and_streams() {
    // Guards the middleware chain's body typing: every response body must
    // still be consumable through actix's standard read helpers.
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get().uri("/healthz").to_request();
    let res = test::call_service(&app, req).await;
    let bytes = actix_web::body::to_bytes(res.into_body()).await.unwrap();
    assert!(!bytes.is_empty());
}
