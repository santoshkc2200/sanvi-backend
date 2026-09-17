//! Integration tests for `GET /api/v1/system/build` (TASK-019 step 1,
//! FR-1102): the probe serves the stamp the binary/state was built with,
//! carries no tenant-scoped field, and is cacheable.

use actix_web::{http::StatusCode, test, web};
use sanvi_system::domain::BuildInfo;

fn test_state() -> sanvi_system::SystemState {
    let build_info = BuildInfo::new(
        "0.1.0-test".into(),
        "deadbeef".into(),
        "2026-09-17T00:00:00Z".into(),
        "test".into(),
        "sanvi-api-test".into(),
    );
    sanvi_system::state_for_tests(build_info, std::time::Duration::from_secs(1))
}

fn test_app() -> actix_web::App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    // The middleware chain is intentionally absent here: the probe must be
    // plain data + a cache header, with no dependency on any middleware.
    actix_web::App::new()
        .app_data(web::Data::new(test_state()))
        .service(web::scope("/api/v1/system").configure(sanvi_system::routes::api_routes))
}

#[actix_web::test]
async fn build_probe_reports_the_stamp_it_was_built_with() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/system/build")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);

    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["commit"], "deadbeef");
    assert_eq!(body["version"], "0.1.0-test");
    assert_eq!(body["built_at"], "2026-09-17T00:00:00Z");
    assert_eq!(body["environment"], "local");
}

/// FR-1102: exactly the four stamp fields — no tenant-scoped field may ever
/// join the response, so the endpoint can stay unauthenticated.
#[actix_web::test]
async fn build_probe_response_has_only_the_stamp_fields() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/system/build")
        .to_request();
    let res = test::call_service(&app, req).await;

    let body: serde_json::Value = test::read_body_json(res).await;
    let mut keys = body
        .as_object()
        .expect("json object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(keys, vec!["built_at", "commit", "environment", "version"]);
}

#[actix_web::test]
async fn build_probe_is_cacheable() {
    let app = test::init_service(test_app()).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/system/build")
        .to_request();
    let res = test::call_service(&app, req).await;
    let cache_control = res
        .headers()
        .get("cache-control")
        .expect("cache-control header")
        .to_str()
        .unwrap();
    assert!(cache_control.contains("public"), "{cache_control}");
    assert!(cache_control.contains("max-age"), "{cache_control}");
}
