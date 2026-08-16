//! Tenant resolution middleware tests: all resolution outcomes and hostile
//! inputs, driven through the assembled middleware chain with a fake
//! resolver (no database).

use std::sync::Arc;

use actix_web::{
    App, HttpResponse,
    dev::{ServiceRequest, ServiceResponse},
    http::StatusCode,
    test, web,
};
use sanvi_http::{
    Resolution, ResolutionSource, TenantContext, TenantMiddleware, TenantPolicyConfig,
    TenantResolver, TenantRuntimeStatus,
};
use uuid::Uuid;

/// A resolver with a scriptable answer.
#[derive(Clone)]
struct FakeResolver {
    answer: Arc<tokio::sync::Mutex<FakeAnswer>>,
    seen_hosts: Arc<std::sync::Mutex<Vec<String>>>,
}

#[derive(Clone)]
enum FakeAnswer {
    Found(TenantContext),
    NotFound,
    Rejected { detail: String },
    Error,
}

impl TenantResolver for FakeResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        internal_header: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Resolution, sanvi_http::ApiError>> + Send + 'a>,
    > {
        self.seen_hosts.lock().unwrap().push(host.to_string());
        let _ = internal_header;
        let answer = self.answer.clone();
        Box::pin(async move {
            match answer.lock().await.clone() {
                FakeAnswer::Found(context) => Ok(Resolution::Found(context)),
                FakeAnswer::NotFound => Ok(Resolution::NotFound),
                FakeAnswer::Rejected { detail } => Ok(Resolution::Rejected { detail }),
                FakeAnswer::Error => Err(sanvi_http::ApiError::service_unavailable("backend down")),
            }
        })
    }
}

fn context(status: TenantRuntimeStatus) -> TenantContext {
    TenantContext {
        tenant_id: Uuid::now_v7(),
        slug: "acme".to_string(),
        display_name: "Acme".to_string(),
        status,
        region: "us".to_string(),
        default_locale: "en".to_string(),
        resolution_source: ResolutionSource::Subdomain,
    }
}

fn build_app(
    resolver: FakeResolver,
    policy: TenantPolicyConfig,
) -> actix_web::App<
    impl actix_web::dev::ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<tracing_actix_web::StreamSpan<actix_web::body::BoxBody>>,
        Error = actix_web::Error,
        InitError = (),
    > + 'static,
> {
    let mut config = sanvi_http::ServerConfig::default();
    config.rate_limit.enabled = false;
    config.tenant = policy.clone();

    let app = App::new()
        .route(
            "/healthz",
            web::get().to(|| async { HttpResponse::Ok().finish() }),
        )
        .route(
            "/api/v1/tenant/context",
            web::get().to(|current: sanvi_http::CurrentTenant| async move {
                HttpResponse::Ok().json(current.0.slug)
            }),
        );

    sanvi_http::middleware::stack(
        app,
        config,
        None,
        Some(Arc::new(TenantMiddleware {
            resolver: Arc::new(resolver),
            policy,
        })),
        None,
    )
}

fn default_policy() -> TenantPolicyConfig {
    TenantPolicyConfig::default()
}

#[actix_web::test]
async fn public_paths_never_require_resolution() {
    let resolver = FakeResolver {
        answer: Arc::new(tokio::sync::Mutex::new(FakeAnswer::NotFound)),
        seen_hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = test::init_service(build_app(resolver.clone(), default_policy())).await;
    let req = test::TestRequest::get().uri("/healthz").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(resolver.seen_hosts.lock().unwrap().is_empty());
}

#[actix_web::test]
async fn resolved_tenant_is_exposed_to_handlers() {
    let resolver = FakeResolver {
        answer: Arc::new(tokio::sync::Mutex::new(FakeAnswer::Found(context(
            TenantRuntimeStatus::Active,
        )))),
        seen_hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = test::init_service(build_app(resolver, default_policy())).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/tenant/context")
        .insert_header(("Host", "acme.platform.sanvi.app"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body, "acme");
}

#[actix_web::test]
async fn unknown_host_on_a_tenant_route_is_a_404_never_a_403() {
    let resolver = FakeResolver {
        answer: Arc::new(tokio::sync::Mutex::new(FakeAnswer::NotFound)),
        seen_hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = test::init_service(build_app(resolver, default_policy())).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/tenant/context")
        .insert_header(("Host", "unknown.example.com"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["type"], "https://sanvi.dev/problems/not-found");
}

#[actix_web::test]
async fn suspended_tenant_gets_423_with_machine_readable_reason() {
    let resolver = FakeResolver {
        answer: Arc::new(tokio::sync::Mutex::new(FakeAnswer::Found(context(
            TenantRuntimeStatus::Suspended,
        )))),
        seen_hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = test::init_service(build_app(resolver, default_policy())).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/tenant/context")
        .insert_header(("Host", "acme.platform.sanvi.app"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::LOCKED);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["type"], "https://sanvi.dev/problems/tenant-suspended");
    assert_eq!(body["reason"], "suspended");
}

#[actix_web::test]
async fn suspended_tenant_allowlist_still_serves() {
    let resolver = FakeResolver {
        answer: Arc::new(tokio::sync::Mutex::new(FakeAnswer::Found(context(
            TenantRuntimeStatus::Suspended,
        )))),
        seen_hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let mut policy = default_policy();
    policy.suspended_allowlist = vec!["/api/v1/billing".to_string()];

    let app = actix_web::App::new().route(
        "/api/v1/billing/invoices",
        web::get().to(|current: sanvi_http::CurrentTenant| async move {
            HttpResponse::Ok().json(current.0.slug)
        }),
    );
    let mut config = sanvi_http::ServerConfig::default();
    config.rate_limit.enabled = false;
    config.tenant = policy.clone();
    let app = sanvi_http::middleware::stack(
        app,
        config,
        None,
        Some(Arc::new(TenantMiddleware {
            resolver: Arc::new(resolver),
            policy,
        })),
        None,
    );
    let app = test::init_service(app).await;

    let req = test::TestRequest::get()
        .uri("/api/v1/billing/invoices")
        .insert_header(("Host", "acme.platform.sanvi.app"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix_web::test]
async fn spoofed_internal_header_is_rejected_with_403() {
    let resolver = FakeResolver {
        answer: Arc::new(tokio::sync::Mutex::new(FakeAnswer::Rejected {
            detail: "token signature mismatch".to_string(),
        })),
        seen_hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = test::init_service(build_app(resolver, default_policy())).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/tenant/context")
        .insert_header(("Host", "acme.platform.sanvi.app"))
        .insert_header(("x-sanvi-tenant", "v1.k1.forged.token"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["type"], "https://sanvi.dev/problems/forbidden");
}

#[actix_web::test]
async fn resolution_backend_failure_never_fails_open() {
    let resolver = FakeResolver {
        answer: Arc::new(tokio::sync::Mutex::new(FakeAnswer::Error)),
        seen_hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = test::init_service(build_app(resolver, default_policy())).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/tenant/context")
        .insert_header(("Host", "acme.platform.sanvi.app"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[actix_web::test]
async fn hostile_host_headers_are_harmless() {
    let resolver = FakeResolver {
        answer: Arc::new(tokio::sync::Mutex::new(FakeAnswer::NotFound)),
        seen_hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = test::init_service(build_app(resolver.clone(), default_policy())).await;
    for hostile in ["a".repeat(1000), "..".to_string(), "%2e%2e".to_string()] {
        let req = test::TestRequest::get()
            .uri("/api/v1/tenant/context")
            .insert_header(("Host", hostile.clone()))
            .to_request();
        let res = test::call_service(&app, req).await;
        assert!(
            res.status() == StatusCode::NOT_FOUND || res.status() == StatusCode::BAD_REQUEST,
            "host {hostile:?} must never pass through"
        );
    }

    // Header injection is rejected at the HTTP layer: a Host containing CRLF
    // cannot even be constructed.
    let injection = std::panic::catch_unwind(|| {
        test::TestRequest::get()
            .uri("/api/v1/tenant/context")
            .insert_header(("Host", "acme.platform.sanvi.app\r\nX-Injected: yes"))
            .to_request()
    });
    assert!(injection.is_err(), "CRLF host injection must be rejected");

    // The resolver still received the hostile hosts (they never bypass it).
    assert_eq!(resolver.seen_hosts.lock().unwrap().len(), 3);
}
