//! Release-candidate corpus captured from real assembled-app responses.
use crate::http_contract::{test_app_with, test_config};
use actix_web::test;
use serde_json::json;

#[actix_web::test]
async fn capture_contract_and_error_journeys() {
    let app = test::init_service(test_app_with(test_config())).await;
    let mut cases = Vec::new();
    for (path, expected_status) in [
        ("/healthz", 200),
        ("/api/v1/system/health", 200),
        ("/api/v1/system/build", 200),
        ("/api/v1/system/version", 200),
        ("/api/v1/system/ready", 503),
        ("/api/v1/nope", 404),
    ] {
        let response =
            test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
        let status = response.status().as_u16();
        assert_eq!(status, expected_status, "replay {path}");
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let body: serde_json::Value = test::read_body_json(response).await;
        let expected_headers = json!({"x-content-type-options": "nosniff", "x-frame-options": "DENY", "referrer-policy": "same-origin",
            "content-security-policy": "default-src 'none'; frame-ancestors 'none'", "permissions-policy": "camera=(), microphone=(), geolocation=(), interest-cohort=()"});
        for (name, expected) in expected_headers.as_object().unwrap() {
            assert_eq!(
                headers.get(name).map(String::as_str),
                expected.as_str(),
                "replay header {path} {name}"
            );
        }
        cases.push(json!({"path": path, "method": "get", "status": status, "headers": headers, "body": body,
            "expected_headers": expected_headers, "generic_problem": path == "/api/v1/nope"}));
    }
    if let Some(path) = std::env::var_os("SANVI_REPLAY_CAPTURE") {
        std::fs::write(path, serde_json::to_vec_pretty(&cases).unwrap()).unwrap();
    }
}
