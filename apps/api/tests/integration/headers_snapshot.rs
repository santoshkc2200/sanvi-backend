//! TASK-024 slice 11.5d: the security-header snapshot (NFR-1114).
//!
//! The backend ships one HTTP app (`apps/api`); "per app" here means every
//! *serving surface* of that app — the probe, the public API JSON, the error
//! path, the docs and ops endpoints, and the cross-origin tracking beacon —
//! because a header that holds on one surface has historically gone missing
//! on another (middleware short-circuits). Each surface below is requested
//! from the fully assembled app and its security headers are compared for
//! **exact** equality against the committed maps: a removed header, a
//! weakened value, or a new header nobody specified all fail here.
//!
//! The HSTS dimension is snapshotted in both postures: absent (local
//! development, plain HTTP) and the staging/production directive.
//!
//! One deliberate, documented non-coverage: a CORS **preflight** response
//! carries none of these headers. The CORS middleware sits *outside* the
//! security-headers middleware and answers `OPTIONS` preflights without
//! calling the service it wraps, so the headers never see the response.
//! That is correct — a preflight has no body a browser renders — and the
//! last test pins it so a future reordering of the stack is a conscious
//! snapshot update, not an accident.

use std::collections::BTreeMap;

use actix_web::test;

use crate::http_contract::{test_app_with, test_config};

/// The header names the snapshot governs. Anything in this set that is
/// missing from a response fails the surface; a value that differs fails
/// it; and the set is compared as a whole, so a header quietly *added* by
/// some future middleware must be specified here before it ships.
const SNAPSHOT_NAMES: [&str; 6] = [
    "content-security-policy",
    "permissions-policy",
    "referrer-policy",
    "strict-transport-security",
    "x-content-type-options",
    "x-frame-options",
];

/// The specified set without HSTS: the local-development posture and the
/// baseline every surface carries in every posture.
fn specified_without_hsts() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'",
        ),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=(), interest-cohort=()",
        ),
        ("referrer-policy", "same-origin"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
    ])
}

/// The staging/production posture: the overlays' HSTS directive, verbatim.
fn specified_with_hsts() -> BTreeMap<&'static str, &'static str> {
    let mut headers = specified_without_hsts();
    headers.insert(
        "strict-transport-security",
        "max-age=31536000; includeSubDomains",
    );
    headers
}

/// Collect the snapshot-governed headers from a response, failing on any
/// that appears more than once (duplicates would let a weakened value hide
/// behind a correct one in some clients).
fn observed(headers: &actix_web::http::header::HeaderMap) -> BTreeMap<String, String> {
    SNAPSHOT_NAMES
        .iter()
        .filter_map(|name| {
            let mut values = headers.get_all(*name);
            let value = values.next()?;
            assert!(
                values.next().is_none(),
                "header {name} appears more than once: a duplicated security \
                 header can hide a weakened value behind a correct one"
            );
            Some((
                (*name).to_string(),
                value.to_str().unwrap_or("<non-ascii>").to_string(),
            ))
        })
        .collect()
}

/// The app config with the staging/production HSTS overlay applied.
fn tls_posture_config() -> sanvi_api::AppConfig {
    let mut config = test_config();
    config.server.security.hsts = Some(sanvi_http::HstsConfig {
        max_age_secs: 31_536_000,
        include_sub_domains: true,
        preload: false,
    });
    config
}

/// Assert one response's snapshot. `expected` is the whole map: equality,
/// not containment, so the snapshot stays the single source of truth.
fn assert_snapshot(
    headers: &actix_web::http::header::HeaderMap,
    status: u16,
    uri: &str,
    expected: &BTreeMap<&str, &str>,
) {
    assert!(
        status < 500,
        "{uri} answered {status}: the snapshot needs a real response, not a server fault"
    );
    assert_eq!(
        observed(headers),
        expected
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>(),
        "security-header snapshot mismatch on {uri} ({status})"
    );
    // No framework fingerprint headers on any surface.
    assert!(
        headers.get("server").is_none(),
        "{uri} leaks a server header"
    );
    assert!(
        headers.get("x-powered-by").is_none(),
        "{uri} leaks an x-powered-by header"
    );
}

/// The serving surfaces the snapshot walks. The tracking beacon is the
/// path tenant storefronts POST to cross-origin; the contract app does not
/// mount advertising, so it answers a router 404 here — what is pinned is
/// the header set on that path, which the middleware applies to router
/// 404s exactly as to handler responses.
const SURFACES: [(actix_web::http::Method, &str); 6] = [
    // Liveness probe.
    (actix_web::http::Method::GET, "/healthz"),
    // Public API JSON (unauthenticated build stamp).
    (actix_web::http::Method::GET, "/api/v1/system/build"),
    // Error path: a 404 problem+json must not bypass the headers.
    (actix_web::http::Method::GET, "/api/v1/nope"),
    // Docs and ops surfaces.
    (actix_web::http::Method::GET, "/openapi.json"),
    (actix_web::http::Method::GET, "/metrics"),
    // The public cross-origin tracking path.
    (actix_web::http::Method::POST, "/api/v1/public/track"),
];

/// Every serving surface of the assembled app, in the local posture:
/// exactly the five specified headers, HSTS absent.
#[actix_web::test]
async fn every_surface_carries_the_specified_set_without_hsts() {
    let app = test::init_service(test_app_with(test_config())).await;
    let expected = specified_without_hsts();
    for (method, uri) in SURFACES {
        let req = test::TestRequest::default()
            .method(method)
            .uri(uri)
            .to_request();
        let res = test::call_service(&app, req).await;
        assert_snapshot(res.headers(), res.status().as_u16(), uri, &expected);
    }
}

/// Every serving surface, in the staging/production posture: the same five
/// headers plus the HSTS directive the overlays configure. A surface that
/// drops only the HSTS header fails here by name.
#[actix_web::test]
async fn every_surface_carries_hsts_in_the_tls_posture() {
    let app = test::init_service(test_app_with(tls_posture_config())).await;
    let expected = specified_with_hsts();
    for (method, uri) in SURFACES {
        let req = test::TestRequest::default()
            .method(method)
            .uri(uri)
            .to_request();
        let res = test::call_service(&app, req).await;
        assert_snapshot(res.headers(), res.status().as_u16(), uri, &expected);
    }
}

/// The pinned non-coverage: preflight responses carry no security headers,
/// because actix-cors short-circuits `OPTIONS` outside the
/// security-headers middleware. A preflight has no renderable body, so
/// this is correct — but it is *specified* here so that reordering the
/// stack (which would put the headers on preflights) is a conscious
/// change to this snapshot, not a silent drift.
#[actix_web::test]
async fn preflight_responses_carry_no_security_headers_by_design() {
    let app = test::init_service(test_app_with(tls_posture_config())).await;
    let req = test::TestRequest::default()
        .method(actix_web::http::Method::OPTIONS)
        .uri("/api/v1/system/build")
        .insert_header(("origin", "https://app.staging.sanvi.dev"))
        .insert_header(("access-control-request-method", "GET"))
        .to_request();
    let res = test::call_service(&app, req).await;
    let headers = observed(res.headers());
    assert!(
        headers.is_empty(),
        "preflight responses gained security headers: {headers:?} — the \
         stack was reordered; update this snapshot deliberately"
    );
}
