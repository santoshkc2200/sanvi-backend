//! The per-app cache-headers wire tests (TASK-022 11.3e).
//!
//! Rules under test, each the kind of thing only a wire test catches:
//! - every response under an admin prefix is `no-store`, overriding any
//!   handler-set value, on any status and method;
//! - a storefront GET without its own `Cache-Control` gains the configured
//!   short shared-cache lifetime;
//! - a handler's own `Cache-Control` on a storefront GET stays
//!   authoritative (the theme endpoint's `public, no-cache`);
//! - non-GET storefront responses and failures off the admin prefixes are
//!   untouched.

use actix_web::{App, HttpResponse, http::Method, http::StatusCode, http::header, test, web};

use sanvi_http::cache_headers::CacheHeadersLayer;
use sanvi_http::config::CacheHeadersConfig;

fn layer() -> CacheHeadersLayer {
    CacheHeadersLayer::new(CacheHeadersConfig {
        storefront_max_age_secs: 60,
    })
}

async fn body_handler() -> HttpResponse {
    HttpResponse::Ok().body("{}")
}

async fn set_cache_control(value: &'static str) -> impl actix_web::Responder {
    HttpResponse::Ok()
        .insert_header(("cache-control", value))
        .body("{}")
}

#[actix_web::test]
async fn admin_responses_are_no_store_on_any_status_and_method() {
    let app = test::init_service(
        App::new()
            .wrap(layer())
            .route("/api/v1/platform/plans", web::get().to(body_handler))
            .route("/api/v1/platform/plans", web::post().to(body_handler))
            .route(
                "/api/v1/platform/nope",
                web::get().to(|| async { HttpResponse::NotFound().finish() }),
            ),
    )
    .await;

    for (method, path, status) in [
        (Method::GET, "/api/v1/platform/plans", StatusCode::OK),
        (Method::POST, "/api/v1/platform/plans", StatusCode::OK),
        (Method::GET, "/api/v1/platform/nope", StatusCode::NOT_FOUND),
    ] {
        let req = test::TestRequest::default()
            .method(method.clone())
            .uri(path)
            .to_request();
        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), status, "{path}");
        assert_eq!(
            res.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "{method} {path} must be no-store"
        );
    }
}

#[actix_web::test]
async fn admin_handler_set_cache_control_is_overridden() {
    let app = test::init_service(App::new().wrap(layer()).route(
        "/api/v1/platform/plans",
        web::get().to(|| set_cache_control("public, max-age=3600")),
    ))
    .await;

    let res = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/platform/plans")
            .to_request(),
    )
    .await;
    assert_eq!(
        res.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "admin no-store overrides the handler's value"
    );
}

#[actix_web::test]
async fn tenant_console_is_admin_for_cache_purposes() {
    let app = test::init_service(
        App::new()
            .wrap(layer())
            .route("/api/v1/tenant/theme", web::get().to(body_handler)),
    )
    .await;
    let res = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/theme")
            .to_request(),
    )
    .await;
    assert_eq!(
        res.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
}

#[actix_web::test]
async fn storefront_gets_the_short_shared_lifetime_when_the_handler_sets_none() {
    let app = test::init_service(
        App::new()
            .wrap(layer())
            .route("/api/v1/public/plans", web::get().to(body_handler)),
    )
    .await;
    let res = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/public/plans")
            .to_request(),
    )
    .await;
    assert_eq!(
        res.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=60")
    );
}

#[actix_web::test]
async fn storefront_handler_cache_control_stays_authoritative() {
    let app = test::init_service(App::new().wrap(layer()).route(
        "/api/v1/public/theme",
        web::get().to(|| set_cache_control("public, no-cache")),
    ))
    .await;
    let res = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/public/theme")
            .to_request(),
    )
    .await;
    assert_eq!(
        res.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("public, no-cache"),
        "a handler's own Cache-Control is never overridden on the storefront"
    );
}

#[actix_web::test]
async fn non_get_and_non_2xx_storefront_responses_are_untouched() {
    let app = test::init_service(
        App::new()
            .wrap(layer())
            .route("/api/v1/public/track", web::post().to(body_handler))
            .route(
                "/api/v1/public/nope",
                web::get().to(|| async { HttpResponse::NotFound().finish() }),
            ),
    )
    .await;

    let post = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/public/track")
            .to_request(),
    )
    .await;
    assert!(!post.headers().contains_key(header::CACHE_CONTROL));

    let not_found = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/public/nope")
            .to_request(),
    )
    .await;
    assert!(!not_found.headers().contains_key(header::CACHE_CONTROL));
}

#[actix_web::test]
async fn non_storefront_2xx_gets_are_untouched() {
    let app = test::init_service(
        App::new()
            .wrap(layer())
            .route("/api/v1/healthz-ish", web::get().to(body_handler)),
    )
    .await;
    let res = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/healthz-ish")
            .to_request(),
    )
    .await;
    assert!(!res.headers().contains_key(header::CACHE_CONTROL));
}
