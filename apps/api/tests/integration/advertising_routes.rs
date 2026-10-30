//! The signed service boundary commits receipts, quota usage and retention evidence atomically.
use actix_web::{App, test, web};
use sanvi_advertising::adapters::remote::{
    AUDIT_AUDIENCE, DELEGATION_HEADER, Delegation, RequestBinding,
};
use sanvi_api::advertising::AdvertisingPlatformState;
use sanvi_kernel::TenantId;
use std::sync::Arc;
const SECRET: &str = "advertising-platform-ingest-test-0123456789";
struct Denied;
impl sanvi_http::FeatureGate for Denied {
    fn check<'a>(
        &'a self,
        _: TenantId,
        _: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<bool, sanvi_http::ApiError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(false) })
    }
}
fn token(path: &str, purpose: &str, body: &[u8]) -> String {
    sanvi_crypto::sign(
        SECRET.as_bytes(),
        &Delegation::new(
            AUDIT_AUDIENCE,
            purpose,
            &RequestBinding::json("POST", path, body),
        ),
    )
}
#[actix_web::test]
async fn event_receipts_deduplicate_conversion_usage_and_retention_runs() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let admin = db.admin_pool().await;
    let tenant = 910_300_100_i64;
    sqlx::query("INSERT INTO tenancy.tenants(id,slug,display_name,status,region) VALUES($1,'ads-ingest','Ads ingest','active','jp')").bind(tenant).execute(&admin).await.unwrap();
    let state = AdvertisingPlatformState {
        secret: sanvi_telemetry::Redacted::new(SECRET.into()),
        transactions: sanvi_persistence::TxManager::new(pool, sanvi_testsupport::test_ids()),
        directives: None,
        feature_gate: Arc::new(Denied),
        enabled: true,
    };
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(sanvi_api::advertising::internal_routes),
    )
    .await;
    let event = |id, event_type: &str, payload| sanvi_messaging::OutboxRecord {
        id: sanvi_kernel::EventId::try_from(id).unwrap(),
        aggregate: "advertising.conversion_event".into(),
        aggregate_id: sanvi_kernel::SnowflakeId::try_from(910_300_101).unwrap(),
        event_type: event_type.into(),
        payload,
        metadata: serde_json::json!({"tenant_id":tenant.to_string()}),
        occurred_at: chrono::Utc::now(),
        published_at: None,
        attempts: 0,
    };
    let conversion = event(
        910_300_102,
        "advertising.conversion_recorded",
        serde_json::json!({}),
    );
    let retention = event(
        910_300_103,
        "advertising.retention_executed",
        serde_json::json!({"data_class":"advertising_metric","action":"delete","partitions_affected":0}),
    );
    for record in [&conversion, &retention] {
        let body = serde_json::to_vec(record).unwrap();
        for _ in 0..2 {
            let request = test::TestRequest::post()
                .uri("/internal/advertising/events")
                .insert_header(("content-type", "application/json"))
                .insert_header((
                    DELEGATION_HEADER,
                    token("/internal/advertising/events", "event", &body),
                ))
                .set_payload(body.clone())
                .to_request();
            assert_eq!(test::call_service(&app, request).await.status(), 204);
        }
    }
    let used: i64 = sqlx::query_scalar("SELECT used FROM platform.quota_counters WHERE tenant_id=$1 AND dimension='tracked_conversions'").bind(tenant).fetch_one(&admin).await.unwrap();
    assert_eq!(used, 1);
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform.advertising_event_receipts")
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(receipts, 2);
    let runs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM privacy.retention_runs WHERE executor='advertising.retention'",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(runs, 1);
    let mut changed = conversion;
    changed.payload = serde_json::json!({"changed":true});
    let body = serde_json::to_vec(&changed).unwrap();
    let request = test::TestRequest::post()
        .uri("/internal/advertising/events")
        .insert_header(("content-type", "application/json"))
        .insert_header((
            DELEGATION_HEADER,
            token("/internal/advertising/events", "event", &body),
        ))
        .set_payload(body)
        .to_request();
    assert_eq!(test::call_service(&app, request).await.status(), 409);
}
#[actix_web::test]
async fn platform_feature_lookup_fails_closed_and_replay_is_rejected() {
    let db = sanvi_testsupport::test_db().await;
    let state = AdvertisingPlatformState {
        secret: sanvi_telemetry::Redacted::new(SECRET.into()),
        transactions: sanvi_persistence::TxManager::new(
            db.app_pool(),
            sanvi_testsupport::test_ids(),
        ),
        directives: None,
        feature_gate: Arc::new(Denied),
        enabled: true,
    };
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(sanvi_api::advertising::internal_routes),
    )
    .await;
    let path = "/internal/advertising/platform";
    let body = serde_json::to_vec(
        &sanvi_advertising::adapters::remote::PlatformRequest::Feature {
            tenant_id: TenantId::try_from(910300100).unwrap(),
            feature: "advertising.budget_guardrails".into(),
        },
    )
    .unwrap();
    let signed = token(path, "platform", &body);
    let request = || {
        test::TestRequest::post()
            .uri(path)
            .insert_header(("content-type", "application/json"))
            .insert_header((DELEGATION_HEADER, signed.clone()))
            .set_payload(body.clone())
            .to_request()
    };
    let response = test::call_service(&app, request()).await;
    assert_eq!(response.status(), 200);
    assert!(!test::read_body_json::<bool, _>(response).await);
    assert_eq!(test::call_service(&app, request()).await.status(), 401);
}
