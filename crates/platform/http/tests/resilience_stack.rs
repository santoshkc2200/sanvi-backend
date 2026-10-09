use actix_web::{App, HttpResponse, test, web};
use sanvi_http::{
    AdmitOutcome, DegradedHeaderLayer, LoadShedState, LoadSheddingConfig, Resolution,
    ResolutionSource, ServerConfig, TenantContext, TenantMiddleware, TenantPolicyConfig,
    TenantResolver, TenantRuntimeStatus,
};
use sanvi_resilience::{DegradationReason, DegradationRegistry};
use std::{future::Future, pin::Pin, sync::Arc};

struct Resolver;

#[actix_web::test]
async fn shipping_webhook_routes_are_exempt_under_redis_degradation() {
    let registry = Arc::new(DegradationRegistry::default());
    registry.set(DegradationReason::Redis, true);
    let state = Arc::new(
        LoadShedState::new(
            LoadSheddingConfig {
                enabled: true,
                in_flight_ceiling: 1,
                ..Default::default()
            },
            None,
        )
        .with_degradation(registry),
    );
    let _held = state.admit("/ordinary", None);
    for path in [
        "/internal/webhooks/stripe/platform",
        "/internal/webhooks/stripe/connect",
        "/internal/kratos/login",
    ] {
        assert!(
            matches!(state.admit(path, None), AdmitOutcome::Admitted(_)),
            "shipping callback must never shed: {path}"
        );
    }
}

#[actix_web::test]
async fn degraded_header_survives_timeout_and_caught_panic() {
    for timeout in [true, false] {
        let registry = Arc::new(DegradationRegistry::default());
        registry.set(DegradationReason::Keto, true);
        let mut config = ServerConfig::default();
        config.rate_limit.enabled = false;
        config.request_timeout_secs = if timeout { 0 } else { 30 };
        let app = test::init_service(sanvi_http::middleware::stack(
            App::new().route(
                "/probe",
                web::get().to(move || async move {
                    if timeout {
                        std::future::pending::<()>().await;
                    }
                    panic!("injected handler panic");
                    #[allow(unreachable_code)]
                    HttpResponse::Ok().finish()
                }),
            ),
            config,
            None,
            None,
            None,
            None,
            None,
            sanvi_http::ssr_cache::SsrCacheLayer::disabled(Default::default()),
            DegradedHeaderLayer::new(registry),
            None,
        ))
        .await;
        let error =
            test::try_call_service(&app, test::TestRequest::get().uri("/probe").to_request())
                .await
                .unwrap_err();
        let response = error.error_response();
        assert_eq!(response.status().as_u16(), if timeout { 504 } else { 500 });
        assert_eq!(response.headers().get("x-sanvi-degraded").unwrap(), "keto");
    }
}
impl TenantResolver for Resolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        _: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, sanvi_http::ApiError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(Resolution::Found(TenantContext {
                tenant_id: sanvi_kernel::TenantId::try_from(if host.starts_with("hot") {
                    101_i64
                } else {
                    102_i64
                })
                .unwrap(),
                slug: host.into(),
                display_name: host.into(),
                status: TenantRuntimeStatus::Active,
                region: "us".into(),
                default_locale: "en".into(),
                resolution_source: ResolutionSource::Subdomain,
            }))
        })
    }
}

#[actix_web::test]
async fn resolved_hot_tenant_is_shed_while_cold_tenant_is_admitted() {
    let state = Arc::new(LoadShedState::new(
        LoadSheddingConfig {
            enabled: true,
            in_flight_ceiling: 4,
            fairness_knee: 1,
            per_tenant_in_flight: 1,
            ..Default::default()
        },
        None,
    ));
    let mut config = ServerConfig::default();
    config.rate_limit.enabled = false;
    let app = test::init_service(sanvi_http::middleware::stack(
        App::new().route(
            "/tenant/probe",
            web::get().to(|| async { HttpResponse::Ok().finish() }),
        ),
        config,
        None,
        Some(Arc::new(TenantMiddleware {
            resolver: Arc::new(Resolver),
            policy: TenantPolicyConfig::default(),
        })),
        None,
        None,
        None,
        sanvi_http::ssr_cache::SsrCacheLayer::disabled(Default::default()),
        DegradedHeaderLayer::disabled(),
        Some(state.clone()),
    ))
    .await;
    let held = state.admit("/tenant/probe", Some("101"));
    assert!(matches!(held, AdmitOutcome::Admitted(_)));
    let hot = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/tenant/probe")
            .insert_header(("Host", "hot.localhost"))
            .to_request(),
    )
    .await;
    assert_eq!(hot.status().as_u16(), 503, "resolved hot tenant must shed");
    let cold = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/tenant/probe")
            .insert_header(("Host", "cold.localhost"))
            .to_request(),
    )
    .await;
    assert_eq!(cold.status().as_u16(), 200);
}

#[actix_web::test]
async fn degraded_header_survives_errors_and_shedding() {
    let registry = Arc::new(DegradationRegistry::default());
    registry.set(DegradationReason::Keto, true);
    let state = Arc::new(LoadShedState::new(
        LoadSheddingConfig {
            enabled: true,
            in_flight_ceiling: 1,
            ..Default::default()
        },
        None,
    ));
    let mut config = ServerConfig::default();
    config.rate_limit.enabled = false;
    let app = test::init_service(sanvi_http::middleware::stack(
        App::new().route(
            "/probe",
            web::get().to(|| async { HttpResponse::InternalServerError().finish() }),
        ),
        config,
        None,
        None,
        None,
        None,
        None,
        sanvi_http::ssr_cache::SsrCacheLayer::disabled(Default::default()),
        DegradedHeaderLayer::new(registry),
        Some(state.clone()),
    ))
    .await;
    let error = test::call_service(&app, test::TestRequest::get().uri("/probe").to_request()).await;
    assert_eq!(error.status().as_u16(), 500);
    assert!(
        error.headers().contains_key("x-sanvi-degraded"),
        "handler error lost degraded header"
    );
    let _held = state.admit("/probe", None);
    let shed = test::call_service(&app, test::TestRequest::get().uri("/probe").to_request()).await;
    assert_eq!(shed.status().as_u16(), 503);
    assert!(
        shed.headers().contains_key("x-sanvi-degraded"),
        "shed response lost degraded header"
    );
}
