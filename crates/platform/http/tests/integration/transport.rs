//! Transport tuning (TASK-021 step 7): the compression floor and the
//! already-compressed exclusion, proven on the wire through the full
//! middleware stack.
//!
//! No Redis/Postgres needed: these drive `test::init_service` with JSON,
//! image and pre-encoded fixtures on both sides of the 1024-byte floor.

use std::io::Read as _;

use actix_web::{App, dev::ServiceResponse, http::StatusCode, test, web};
use sanvi_http::{ServerConfig, middleware};

fn test_config() -> ServerConfig {
    let mut config = ServerConfig::default();
    config.rate_limit.enabled = false;
    config
}

fn build_app(
    config: ServerConfig,
) -> App<
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
    let app = App::new()
        .route("/small", web::get().to(small_json))
        .route("/sized", web::get().to(sized_json))
        .route("/large", web::get().to(large_json))
        .route("/image", web::get().to(png_bytes))
        .route("/precompressed", web::get().to(precompressed));
    middleware::stack(app, config, None, None, None, None, None)
}

/// ~200 B of JSON: under the floor, never encoded.
async fn small_json() -> impl actix_web::Responder {
    actix_web::HttpResponse::Ok().json(serde_json::json!({"ok": true}))
}

/// Exactly `n` bytes of `application/json`.
async fn sized_json(
    query: web::Query<std::collections::HashMap<String, String>>,
) -> impl actix_web::Responder {
    let n: usize = query.get("n").and_then(|n| n.parse().ok()).unwrap_or(0);
    actix_web::HttpResponse::Ok()
        .content_type("application/json")
        .body("a".repeat(n))
}

/// ~4 KiB of JSON: above the floor, encodes.
async fn large_json() -> impl actix_web::Responder {
    actix_web::HttpResponse::Ok()
        .json(serde_json::json!({"items": vec!["payload-".to_string() + &"x".repeat(64); 48]}))
}

/// 4 KiB of already-compressed bytes served as PNG: above the floor, but
/// the type says encoding it is waste.
async fn png_bytes() -> impl actix_web::Responder {
    actix_web::HttpResponse::Ok()
        .content_type("image/png")
        .body(vec![0x89u8; 4096])
}

/// 4 KiB the handler already encoded: the middleware must not touch it —
/// double compression would also lie to the client about the framing.
async fn precompressed() -> impl actix_web::Responder {
    actix_web::HttpResponse::Ok()
        .insert_header(("content-encoding", "gzip"))
        .content_type("application/json")
        .body(vec![0x1fu8; 4096])
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("the response must be valid gzip");
    out
}

fn bunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::Decompressor::new(bytes, 4096)
        .read_to_end(&mut out)
        .expect("the response must be valid brotli");
    out
}

fn content_encoding<B>(res: &ServiceResponse<B>) -> Option<String> {
    res.headers()
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[actix_web::test]
async fn small_payloads_pass_through_identity() {
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get()
        .uri("/small")
        .insert_header(("accept-encoding", "gzip, deflate, br"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(content_encoding(&res), None);
    let body = test::read_body(res).await;
    assert_eq!(body, r#"{"ok":true}"#);
}

#[actix_web::test]
async fn the_floor_boundary_is_exact() {
    // 1023 passes through, 1024 encodes: the gate is `>= min_bytes`, and
    // this test pins which side the boundary byte falls on.
    let app = test::init_service(build_app(test_config())).await;

    let req = test::TestRequest::get()
        .uri("/sized?n=1023")
        .insert_header(("accept-encoding", "gzip"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(content_encoding(&res), None);
    assert_eq!(test::read_body(res).await.len(), 1023);

    for n in [1024, 1025] {
        let req = test::TestRequest::get()
            .uri(&format!("/sized?n={n}"))
            .insert_header(("accept-encoding", "gzip"))
            .to_request();
        let res = test::call_service(&app, req).await;
        assert_eq!(content_encoding(&res).as_deref(), Some("gzip"), "n={n}");
        let body = test::read_body(res).await;
        assert_eq!(gunzip(&body), vec![b'a'; n], "n={n}");
    }
}

#[actix_web::test]
async fn large_json_negotiates_brotli_first() {
    // At equal q the server ranking is brotli > zstd > gzip > deflate.
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get()
        .uri("/large")
        .insert_header(("accept-encoding", "gzip, deflate, br, zstd"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(content_encoding(&res).as_deref(), Some("br"));
    assert!(
        res.headers().get("vary").is_some_and(|vary| vary
            .to_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("accept-encoding")),
        "an encoded response must carry Vary: Accept-Encoding"
    );
    let body = test::read_body(res).await;
    assert!(body.len() < 4096, "brotli must actually shrink the payload");
    let _ = bunzip(&body);
}

#[actix_web::test]
async fn large_json_gzip_round_trips() {
    let app = test::init_service(build_app(test_config())).await;
    let plain = test::init_service(build_app(test_config())).await;
    let expected = test::read_body(
        test::call_service(&plain, test::TestRequest::get().uri("/large").to_request()).await,
    )
    .await;

    let req = test::TestRequest::get()
        .uri("/large")
        .insert_header(("accept-encoding", "gzip"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(content_encoding(&res).as_deref(), Some("gzip"));
    let body = test::read_body(res).await;
    assert_eq!(gunzip(&body), expected);
}

#[actix_web::test]
async fn refused_gzip_falls_back_to_identity() {
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get()
        .uri("/large")
        .insert_header(("accept-encoding", "gzip;q=0"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(content_encoding(&res), None);
}

#[actix_web::test]
async fn missing_accept_encoding_means_identity() {
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get().uri("/large").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(content_encoding(&res), None);
}

#[actix_web::test]
async fn already_compressed_types_pass_through() {
    // 4 KiB of PNG sits far above the floor; the type alone refuses it.
    // (The stock `Compress` middleware encodes this — the exclusion is the
    // difference this slice ships.)
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get()
        .uri("/image")
        .insert_header(("accept-encoding", "gzip, deflate, br"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(content_encoding(&res), None);
    assert_eq!(test::read_body(res).await, vec![0x89u8; 4096]);
}

#[actix_web::test]
async fn already_encoded_payloads_are_never_recompressed() {
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get()
        .uri("/precompressed")
        .insert_header(("accept-encoding", "gzip, deflate, br"))
        .to_request();
    let res = test::call_service(&app, req).await;
    // Still exactly one framing, and the bytes are the handler's own.
    assert_eq!(content_encoding(&res).as_deref(), Some("gzip"));
    assert_eq!(test::read_body(res).await, vec![0x1fu8; 4096]);
}

#[actix_web::test]
async fn refusing_everything_gets_406_without_running_the_handler() {
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get()
        .uri("/large")
        .insert_header(("accept-encoding", "identity;q=0"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::NOT_ACCEPTABLE);
    assert!(res.headers().get("vary").is_some());
}

#[actix_web::test]
async fn disabled_compression_is_the_rollback() {
    let mut config = test_config();
    config.compression.enabled = false;
    let app = test::init_service(build_app(config)).await;
    let req = test::TestRequest::get()
        .uri("/large")
        .insert_header(("accept-encoding", "gzip, deflate, br"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(content_encoding(&res), None);
}

#[actix_web::test]
async fn successful_gets_carry_a_validator_and_revalidate() {
    let app = test::init_service(build_app(test_config())).await;

    let req = test::TestRequest::get().uri("/large").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let etag = res
        .headers()
        .get("etag")
        .expect("a successful GET carries an ETag")
        .to_str()
        .unwrap()
        .to_string();
    let body = test::read_body(res).await;
    assert!(!body.is_empty());

    // The same validator comes back: 304, empty body, ETag echoed.
    let req = test::TestRequest::get()
        .uri("/large")
        .insert_header(("if-none-match", etag.clone()))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(res.headers().get("etag").unwrap().to_str().unwrap(), etag);
    assert!(test::read_body(res).await.is_empty());

    // `*` matches any current representation.
    let req = test::TestRequest::get()
        .uri("/large")
        .insert_header(("if-none-match", "*"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);

    // A stale validator is just a GET.
    let req = test::TestRequest::get()
        .uri("/large")
        .insert_header(("if-none-match", "\"stale\""))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(test::read_body(res).await, body);
}
