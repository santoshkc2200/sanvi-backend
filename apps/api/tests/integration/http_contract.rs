//! Contract tests: exercise the assembled `App` through HTTP.
//!
//! No live infrastructure: readiness probes are fakes (one healthy, one
//! degradable), so these tests cover the middleware stack, problem+json
//! shape, headers, rate limiting (disabled) and the OpenAPI endpoint.

use actix_web::{http::StatusCode, test};
use sanvi_api::{
    AccessEntitlementAdmin, AppConfig, IdentityDependencies, PlatformAdminDependencies,
    TenantCatalogAdapter, build_access_state, build_app, build_audit_state, build_identity_state,
    build_platform_admin_state, build_system_state, build_theming_state,
};
use sanvi_system::domain::BuildInfo;
use sanvi_telemetry::MetricsRegistry;
use sanvi_tenancy::adapters::http::state_for_tests as tenancy_state_for_tests;
use sanvi_testsupport::test_ids;

/// A lazy pool that never connects: the readiness probe is not part of this
/// test suite (covered by the context's own tests). The acquire window is
/// one second on purpose — sqlx otherwise retries the refused connects for
/// its 30 s default, pushing every storage-touching request into the
/// request-budget middleware's own 30 s timeout and turning
/// `cache_headers_snapshot` into a coin flip.
fn pool_stub() -> sanvi_persistence::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(1))
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .expect("lazy pool construction never connects")
}

/// Shared with `headers_snapshot.rs`: the snapshot needs the same assembled
/// app, and duplicating a hundred lines of wiring would drift.
pub(crate) fn test_config() -> AppConfig {
    let mut config = AppConfig::default();
    config.telemetry.service_name = "sanvi-api-test".to_string();
    config.server.rate_limit.enabled = false;
    config
}

/// The assembled app with the tenant middleware off (default: every route
/// behaves public) or on (`features.tenancy`, the production posture).
fn test_app_with_tenancy(
    tenancy: bool,
) -> actix_web::App<
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
    let mut config = test_config();
    config.features.tenancy = tenancy;
    test_app_with(config)
}

/// The assembled app for an arbitrary config. Identity wiring is real
/// adapter construction against the lazy pool; with
/// `features.identity` on, session verification dials the (unreachable
/// in tests) Kratos URL, which is exactly the incident posture some
/// contracts are about.
///
/// Shared with `headers_snapshot.rs` (see `test_config`).
pub(crate) fn test_app_with(
    config: AppConfig,
) -> actix_web::App<
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
    let metrics = MetricsRegistry::prometheus_only();
    let build_info = BuildInfo::new(
        "0.1.0-test".into(),
        "test".into(),
        "2026-10-09T00:00:00Z".into(),
        "test".into(),
        "sanvi-api-test".into(),
    );
    let system = build_system_state(build_info, "test", pool_stub(), None, 1);

    // Identity/access wiring: real adapter construction with the lazy pool
    // (nothing connects until a route actually exercises it).
    let pool = pool_stub();
    let ids = test_ids();
    let role_catalog: std::sync::Arc<dyn sanvi_identity::domain::RoleCatalog> =
        std::sync::Arc::new(sanvi_access::adapters::SqlxRoleCatalog::new(
            sanvi_persistence::TxManager::new(pool.clone(), ids.clone()),
            std::sync::Arc::new(sanvi_access::adapters::SqlxRoleRepository::new()),
        ));
    let tenant_catalog: std::sync::Arc<dyn sanvi_identity::domain::TenantCatalog> =
        std::sync::Arc::new(TenantCatalogAdapter::new(
            sanvi_persistence::TxManager::new(pool.clone(), ids.clone()),
            std::sync::Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
        ));
    let identity = build_identity_state(
        pool.clone(),
        ids.clone(),
        None,
        &config.identity,
        &config.ory.kratos_public_url,
        &config.ory.kratos_admin_url,
        IdentityDependencies {
            role_catalog,
            tenant_catalog,
            notification_localizer: None,
            notification_gateway: None,
        },
    );
    let audit = build_audit_state(
        pool.clone(),
        ids.clone(),
        &config.audit,
        config.features.archival,
    );
    let access = build_access_state(
        pool.clone(),
        ids.clone(),
        &config.access,
        &config.ory.keto_read_url,
        &config.ory.keto_write_url,
        identity.memberships.clone(),
        audit.writer.clone(),
        build_approval_gate_stub(),
        metrics.prometheus_registry(),
        None,
        std::sync::Arc::new(sanvi_resilience::DegradationRegistry::default()),
    );

    // Theming wired for real: its public route shares the /api/v1/public
    // prefix with tenancy's, and the routing contract below needs both
    // mounted in one router (the lazy pool answers nothing, which is
    // fine — that contract pins routing, not storage).
    let tenancy = tenancy_state_for_tests(test_ids());
    let theming = build_theming_state(
        pool.clone(),
        test_ids(),
        &config.theming,
        metrics.prometheus_registry(),
        None,
        access.domain_feature_gate.clone(),
        tenancy.host_resolver.clone(),
    );

    let entitlement_admin: std::sync::Arc<dyn sanvi_platform_admin::domain::EntitlementAdmin> =
        std::sync::Arc::new(AccessEntitlementAdmin::new(
            access.grant_entitlement.clone(),
            access.revoke_entitlement.clone(),
            access.list_tenant_overrides.clone(),
        ));
    let platform_admin = build_platform_admin_state(
        pool.clone(),
        ids,
        &config.platform_admin,
        PlatformAdminDependencies {
            tenants: std::sync::Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
            memberships: identity.memberships.clone(),
            entitlements: std::sync::Arc::new(
                sanvi_access::adapters::SqlxEntitlementRepository::new(),
            ),
            entitlement_admin,
            audit_writer: audit.writer.clone(),
            privacy_lock: std::sync::Arc::new(sanvi_platform_admin::adapters::NoopPrivacyLock),
        },
    );

    build_app(
        config,
        metrics,
        system,
        tenancy,
        identity,
        access,
        audit,
        platform_admin,
        None, // billing
        None, // privacy
        None, // localization
        Some(theming),
        None,                                                                  // domains
        None,                                                                  // payments
        None,                                                                  // advertising
        None,                                                                  // accounting
        None,                                                                  // redis
        None,                                                                  // ssr_cache
        std::sync::Arc::new(sanvi_resilience::DegradationRegistry::default()), // degradation
        None, // load_shed: shedding tests build their own stack
    )
}

/// The four-eyes gate for contract tests: never requires approval (the
/// endpoints that would record approvals are not exercised here).
fn build_approval_gate_stub() -> std::sync::Arc<dyn sanvi_access::domain::ApprovalGate> {
    std::sync::Arc::new(sanvi_platform_admin::adapters::NoopApprovalGate)
}

#[actix_web::test]
async fn healthz_is_live() {
    let app = test::init_service(test_app_with_tenancy(false)).await;
    let req = test::TestRequest::get().uri("/healthz").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["status"], "ok");
}

#[actix_web::test]
async fn readyz_degrades_when_a_dependency_is_down() {
    // The stub pool cannot connect: readiness must answer 503 problem+json.
    let app = test::init_service(test_app_with_tenancy(false)).await;
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
    let app = test::init_service(test_app_with_tenancy(false)).await;
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
async fn build_probe_stamps_the_build_without_tenant_data() {
    let app = test::init_service(test_app_with_tenancy(false)).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/system/build")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().get("cache-control").is_some());

    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["commit"], "test");
    assert_eq!(body["version"], "0.1.0-test");
    assert_eq!(body["environment"], "test");

    // Exactly the four stamp fields (FR-1102): no tenant-scoped field may
    // ever join, so the endpoint can stay unauthenticated.
    let mut keys = body
        .as_object()
        .expect("json object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(keys, vec!["built_at", "commit", "environment", "version"]);
}

/// With tenant resolution actually wired (the flag on), the build probe
/// must still answer without a tenant host — it is the unauthenticated
/// stamp RUM and the error tracker read — while the sibling version
/// endpoint keeps its tenant-scoped behaviour.
#[actix_web::test]
async fn build_probe_is_public_under_active_tenant_middleware() {
    let app = test::init_service(test_app_with_tenancy(true)).await;

    let req = test::TestRequest::get()
        .uri("/api/v1/system/build")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);

    let req = test::TestRequest::get()
        .uri("/api/v1/system/version")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// The incident posture: identity wired (`features.identity`, so the authn
/// middleware is live) and the identity backend unreachable (Kratos is
/// down in tests, as in a real incident). A same-origin caller — browser
/// RUM, a status page — carries the session cookie, so if the probe ran
/// session verification it would fail closed with 503 exactly when the
/// incident-time stamp is needed most. The probe must be authn-exempt and
/// answer 200 regardless.
#[actix_web::test]
async fn build_probe_answers_while_the_identity_backend_is_down() {
    let mut config = test_config();
    config.features.identity = true;
    let cookie_name = config.identity.session_cookie_name.clone();
    let app = test::init_service(test_app_with(config)).await;

    let req = test::TestRequest::get()
        .uri("/api/v1/system/build")
        .cookie(actix_web::cookie::Cookie::new(cookie_name, "stale-session"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "the build probe must not run session verification: an unreachable \
         identity backend must not 503 the incident-time stamp"
    );
}

#[actix_web::test]
async fn unknown_route_is_a_404_problem() {
    let app = test::init_service(test_app_with_tenancy(false)).await;
    let req = test::TestRequest::get().uri("/api/v1/nope").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert!(body.get("type").is_some());
    assert!(body.get("status").is_some());
}

/// The routing pin for the actix scope-shadowing outage: several contexts
/// contribute public routes under `/api/v1/public`, and registering that
/// prefix as a `web::scope` commits the router to the first matching
/// scope without backtracking, so every later context's public routes
/// 404 — invisible while each context was tested against its own router,
/// and invisible in OpenAPI, which stayed green through the outage. Both
/// contexts are wired into one router here.
///
/// A GET cannot do the pinning: the theme handler itself answers 404 for
/// a host it cannot resolve, and that handler-404 is indistinguishable
/// by status from the router-404 the scope bug produces. A method the
/// route does not serve is unambiguous — on a registered resource actix
/// answers 405, while under the scope bug the request never reaches a
/// resource and the answer stays 404.
#[actix_web::test]
async fn every_contexts_public_routes_resolve_under_one_router() {
    let mut config = test_config();
    config.features.tenancy = true;
    let app = test::init_service(test_app_with(config)).await;

    for uri in ["/api/v1/public/tenant-context", "/api/v1/public/theme"] {
        let req = test::TestRequest::post().uri(uri).to_request();
        let res = test::call_service(&app, req).await;
        assert_ne!(
            res.status(),
            StatusCode::NOT_FOUND,
            "POST {uri} must reach a registered resource (405): a 404 here is \
             the scope-shadowing regression, where actix commits to the first \
             context's /api/v1/public scope and later contexts' routes vanish"
        );
    }
}

#[actix_web::test]
async fn responses_carry_security_headers_and_request_id() {
    let app = test::init_service(test_app_with_tenancy(false)).await;
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
    let app = test::init_service(test_app_with_tenancy(false)).await;
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
    let app = test::init_service(test_app_with_tenancy(false)).await;
    let req = test::TestRequest::get().uri("/openapi.json").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["info"]["title"], "Sanvi API");
    assert!(body["paths"].get("/healthz").is_some());
}

#[actix_web::test]
async fn openapi_json_is_pre_rendered_and_conditional() {
    // TASK-021 11.2h: the merged document is static per build, so it is
    // rendered once (two reads are byte-identical) and carries a strong
    // validator — a revalidation costs headers, not a rebuild.
    let app = test::init_service(test_app_with_tenancy(false)).await;

    let req = test::TestRequest::get().uri("/openapi.json").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let etag = res
        .headers()
        .get("etag")
        .expect("the contract carries a validator")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        res.headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap(),
        "public, no-cache"
    );
    let first = test::read_body(res).await;
    // The floor applies to the API's largest static asset: a contract that
    // shrank under it would silently stop being compressed.
    assert!(
        first.len() > sanvi_http::CompressionConfig::default().min_bytes,
        "the contract must stay above the compression floor ({} bytes)",
        first.len()
    );

    let req = test::TestRequest::get().uri("/openapi.json").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(test::read_body(res).await, first);

    let req = test::TestRequest::get()
        .uri("/openapi.json")
        .insert_header(("if-none-match", etag.clone()))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(res.headers().get("etag").unwrap().to_str().unwrap(), etag);
    assert!(test::read_body(res).await.is_empty());
}

#[actix_web::test]
async fn metrics_endpoint_renders_prometheus_format() {
    let app = test::init_service(test_app_with_tenancy(false)).await;
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
    let app = test::init_service(test_app_with_tenancy(false)).await;
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
    let app = test::init_service(test_app_with_tenancy(false)).await;
    let req = test::TestRequest::get().uri("/healthz").to_request();
    let res = test::call_service(&app, req).await;
    let bytes = actix_web::body::to_bytes(res.into_body()).await.unwrap();
    assert!(!bytes.is_empty());
}
