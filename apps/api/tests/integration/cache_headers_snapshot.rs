//! TASK-022 slice 11.3e: the cache-header snapshot per serving surface.
//!
//! Extends the 11.5d security-header snapshot idea to the cache policy:
//! each surface is requested from the fully assembled app and its
//! `Cache-Control` / `Vary` set compared against the documented policy —
//! admin (`/api/v1/platform`, `/api/v1/tenant`) always `no-store`, the
//! anonymous storefront reads carrying the configured short shared-cache
//! lifetime when the handler sets none, and the docs endpoint keeping its
//! own `public, no-cache`. `Vary: Accept-Language` on locale-dependent
//! responses is stamped by the locale middleware and pinned through the
//! full stack by sanvi-http's `locale_resolution.rs`
//! (`vary_header_merges_with_existing_values`); the harness here mounts no
//! localization wiring, so it is not re-asserted on this assembly.

use actix_web::{http::header::HeaderMap, test};

use crate::http_contract::{test_app_with, test_config};

fn cache_control(headers: &HeaderMap) -> Option<&str> {
    headers.get("cache-control").and_then(|v| v.to_str().ok())
}

#[actix_web::test]
async fn cache_headers_match_the_per_app_policy() {
    let app = test::init_service(test_app_with(test_config())).await;

    // Platform admin, unauthenticated (401): still no-store — the policy
    // holds on every status.
    let res = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/platform/admin/tenants")
            .to_request(),
    )
    .await;
    assert_eq!(res.status(), 401);
    assert_eq!(
        cache_control(res.headers()),
        Some("no-store"),
        "platform admin surface"
    );

    // Tenant console, unauthenticated: the request fails tenant resolution
    // before auth (404 — no tenant for the test host), and the no-store
    // policy holds on that status too. That is the point: *every* status.
    let res = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/theme/draft")
            .to_request(),
    )
    .await;
    assert_eq!(res.status(), 404);
    assert_eq!(
        cache_control(res.headers()),
        Some("no-store"),
        "tenant console surface"
    );

    // The docs endpoint keeps its TASK-021 posture (a 200 on a
    // non-storefront path: fill-if-absent never fires, the handler's own
    // Cache-Control stands).
    let res = test::call_service(
        &app,
        test::TestRequest::get().uri("/openapi.json").to_request(),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(
        cache_control(res.headers()),
        Some("public, no-cache"),
        "openapi.json"
    );

    // Storefront read: the harness mounts only theming's public route, and
    // its storage fails on the lazy pool (500) — the cache policy must stay
    // hands-off on a non-2xx, while the locale middleware's
    // `Vary: Accept-Language` is stamped on every response it sees. The
    // `public, max-age` fill on 2xx storefront GETs is pinned by the wire
    // tests in sanvi-http (`tests/integration/cache_headers.rs`).
    let res = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/public/theme")
            .insert_header(("host", "shop.tenant.example"))
            .to_request(),
    )
    .await;
    assert_eq!(res.status(), 500, "lazy pool: theming storage fails");
    assert_eq!(
        cache_control(res.headers()),
        None,
        "no cache header is invented for a non-2xx storefront read"
    );

    // The theme endpoint manages its own validator and policy: when it
    // answers 200 it sets `public, no-cache` itself, and the cache layer
    // never overrides it (asserted in sanvi-http's wire tests).

    // The error path off the governed prefixes carries no cache headers.
    let res = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/v1/nope").to_request(),
    )
    .await;
    assert_eq!(res.status(), 404);
    assert_eq!(
        cache_control(res.headers()),
        None,
        "unrouted 404 is untouched"
    );
}
