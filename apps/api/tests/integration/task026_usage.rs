//! The registered tenancy usage route and HTTP quota layer share durable counts.
use std::{future::Future, pin::Pin, sync::Arc};

use actix_web::{
    App, HttpMessage, HttpResponse,
    dev::{Service, ServiceResponse},
    http::StatusCode,
    test, web,
};
use async_trait::async_trait;
use sanvi_http::{
    ApiError, CurrentSubject, Decision, PermissionGate, ResolutionSource, ResourceRef, Subject,
    TenantContext, TenantRuntimeStatus,
    quota_middleware::{QuotaLayer, QuotaRuntime},
    quotas::{QuotaDimension, QuotaError, QuotaPlan, QuotaPlanProvider, QuotaService, TenantUsage},
};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;

fn replay_case<B>(
    response: &ServiceResponse<B>,
    path: &str,
    generic_problem: bool,
) -> serde_json::Value {
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    serde_json::json!({"path": path, "method": "get", "status": response.status().as_u16(),
        "headers": headers, "expected_headers": {}, "generic_problem": generic_problem})
}

struct SmallPlan;
#[async_trait]
impl QuotaPlanProvider for SmallPlan {
    async fn plan_for(&self, _: TenantId) -> Result<QuotaPlan, QuotaError> {
        let mut plan = QuotaPlan::for_tier("starter")?;
        plan.limits.insert(QuotaDimension::ApiRequests, 7);
        Ok(plan)
    }
}

struct FixturePermissionGate;
impl PermissionGate for FixturePermissionGate {
    fn check<'a>(
        &'a self,
        subject: &'a Subject,
        permission: &'a str,
        resource: &'a ResourceRef,
        _write: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, ApiError>> + Send + 'a>> {
        assert_eq!(permission, "tenancy.settings.read");
        assert_eq!(
            *resource,
            ResourceRef::Tenant(TenantId::try_from(700_i64).unwrap())
        );
        Box::pin(async move {
            Ok(if subject.session_id == "verified-allowed" {
                Decision::Allowed
            } else {
                Decision::Denied
            })
        })
    }
}

#[actix_web::test]
async fn tenant_usage_matches_quota_layer_and_recovery_preserves_authentication() {
    let db = sanvi_testsupport::test_db().await;
    sqlx::query(
        "INSERT INTO tenancy.tenants(id,slug,display_name,status,region)
        VALUES(700,'usage-http','HTTP usage','active','jp')",
    )
    .execute(&db.admin_pool().await)
    .await
    .unwrap();
    let tenant = TenantId::try_from(700_i64).unwrap();
    let tx = TxManager::new(db.app_pool(), sanvi_testsupport::test_ids());
    let quota = Arc::new(QuotaService::new(tx, Arc::new(SmallPlan), None));
    quota
        .record(tenant, QuotaDimension::ApiRequests, 6, false)
        .await
        .unwrap();
    let mut tenancy = sanvi_tenancy::adapters::http::state_for_tests(sanvi_testsupport::test_ids());
    tenancy.quotas = Some(quota.clone());
    let gate: Arc<dyn PermissionGate> = Arc::new(FixturePermissionGate);
    let mut replay_cases = Vec::new();
    let app = test::init_service(App::new()
        .app_data(web::Data::new(tenancy))
        .app_data(web::Data::new(gate))
        .app_data(web::Data::new(QuotaRuntime { service: quota.clone(), enabled: true }))
        .service(web::scope("/api/v1/tenant")
        .configure(sanvi_tenancy::adapters::http::tenant_scope_routes)
        .route("/test-metered", web::get().to(|_: CurrentSubject| async { HttpResponse::Ok().finish() }))
        // The billing stub pins QuotaLayer's recovery classification. Billing
        // provider behavior has its own contract suite.
        .route("/billing/checkout-session", web::post().to(|| async { HttpResponse::NoContent().finish() })))
        .wrap(sanvi_http::error::ErrorHandlersLayer)
        .wrap(QuotaLayer)
        .wrap_fn(move |request, service| {
            request.extensions_mut().insert(TenantContext { tenant_id: tenant,
                slug: "usage-http".into(), display_name: "HTTP usage".into(),
                status: TenantRuntimeStatus::Active, region: "jp".into(), default_locale: "en".into(),
                resolution_source: ResolutionSource::SessionClaim });
            // Trusted test session fixture substitutes the identity verifier;
            // the production CurrentSubject/RequiredPermission extractors run.
            if let Some(session) = request.headers().get("x-test-session") {
                let session_id = session.to_str().unwrap().to_owned();
                let subject_tenant = if session_id == "verified-other-tenant" {
                    TenantId::try_from(701_i64).unwrap()
                } else { tenant };
                request.extensions_mut().insert(Subject { user_id: UserId::try_from(701_i64).unwrap(),
                    session_id, aal: "aal1".into(), methods: vec!["password".into()],
                    tenant_id: Some(subject_tenant), role_ids: Vec::new(), authenticated_at: None });
            }
            service.call(request)
        })).await;

    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/test-metered")
            .insert_header(("x-test-session", "verified-allowed"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/test-metered")
            .insert_header(("x-test-session", "verified-allowed"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let mut breach_case = replay_case(&response, "/api/v1/tenant/test-metered", true);
    let breach: serde_json::Value = test::read_body_json(response).await;
    breach_case["body"] = breach.clone();
    replay_cases.push(breach_case);
    assert_eq!(breach["dimension"], "api_requests");
    assert_eq!(breach["used"], 8);
    assert_eq!(breach["limit"], 7);
    assert!(
        breach["upgrade_path"]
            .as_str()
            .unwrap()
            .contains("professional")
    );

    // Tenant resolution alone must not let anonymous requests consume the
    // authenticated tenant's quota or mask the handler's authentication error.
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/test-metered")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut anonymous_case = replay_case(&response, "/api/v1/tenant/test-metered", true);
    anonymous_case["body"] = test::read_body_json::<serde_json::Value, _>(response).await;
    replay_cases.push(anonymous_case);
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::ApiRequests].used,
        8
    );

    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/test-metered")
            .insert_header(("x-test-session", "verified-other-tenant"))
            .to_request(),
    )
    .await;
    // CurrentSubject accepts the verified fixture; tenant authorization belongs
    // to the real resolver. QuotaLayer must leave that decision to its consumer.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::ApiRequests].used,
        8
    );

    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/usage")
            .insert_header(("x-test-session", "verified-allowed"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut usage_case = replay_case(&response, "/api/v1/tenant/usage", false);
    let usage_body: serde_json::Value = test::read_body_json(response).await;
    let usage: TenantUsage = serde_json::from_value(usage_body.clone()).unwrap();
    usage_case["body"] = usage_body;
    replay_cases.push(usage_case);
    assert_eq!(usage.dimensions[&QuotaDimension::ApiRequests].used, 9);
    assert_eq!(usage.dimensions[&QuotaDimension::ApiRequests].limit, 7);
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::ApiRequests].used,
        9
    );
    assert!(matches!(
        quota.enforce(tenant, QuotaDimension::ApiRequests).await,
        Err(QuotaError::Exceeded {
            used: 9,
            limit: 7,
            ..
        })
    ));

    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/tenant/billing/checkout-session")
            .insert_header(("x-test-session", "verified-allowed"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/usage")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut authentication_case = replay_case(&response, "/api/v1/tenant/usage", true);
    authentication_case["body"] = test::read_body_json::<serde_json::Value, _>(response).await;
    replay_cases.push(authentication_case);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/usage")
            .insert_header(("x-test-session", "verified-denied"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let mut permission_case = replay_case(&response, "/api/v1/tenant/usage", true);
    let denial: serde_json::Value = test::read_body_json(response).await;
    assert!(
        denial["detail"]
            .as_str()
            .unwrap()
            .contains("tenancy.settings.read")
    );
    assert!(denial.get("upgrade_path").is_none());
    permission_case["body"] = denial;
    replay_cases.push(permission_case);
    if let Some(mut path) = std::env::var_os("SANVI_REPLAY_CAPTURE") {
        path.push(".usage.json");
        std::fs::write(path, serde_json::to_vec_pretty(&replay_cases).unwrap()).unwrap();
    }
}
