//! Durable quota regression tests: real PostgreSQL and isolated Redis keys.
use async_trait::async_trait;
use sanvi_http::quotas::{QuotaDimension, QuotaError, QuotaPlan, QuotaPlanProvider, QuotaService};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use std::sync::Arc;

struct SmallPlan;

#[actix_web::test]
async fn quota_boundary_denials_are_correlated_problem_json_with_quota_fields() {
    use actix_web::{App, HttpMessage, HttpResponse, http::StatusCode, test, web};
    use sanvi_http::{
        Subject, TenantContext,
        quota_middleware::{QuotaLayer, QuotaRuntime},
        tenancy::{ResolutionSource, TenantRuntimeStatus},
    };
    let (_db, tx, tenant) = fixture().await;
    let service = Arc::new(QuotaService::new(tx, Arc::new(SmallPlan), None));
    service
        .record(tenant, QuotaDimension::ApiRequests, 7, false)
        .await
        .unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(QuotaRuntime {
                service,
                enabled: true,
            }))
            .wrap(QuotaLayer)
            .wrap(actix_web::middleware::from_fn(
                move |req: actix_web::dev::ServiceRequest,
                      next: actix_web::middleware::Next<actix_web::body::BoxBody>| async move {
                    req.extensions_mut().insert(TenantContext {
                        tenant_id: tenant,
                        slug: "quota-test".into(),
                        display_name: "Quota test".into(),
                        status: TenantRuntimeStatus::Active,
                        region: "jp".into(),
                        default_locale: "en".into(),
                        resolution_source: ResolutionSource::SessionClaim,
                    });
                    req.extensions_mut().insert(Subject {
                        user_id: sanvi_kernel::UserId::try_from(1_i64).unwrap(),
                        session_id: "verified-session".into(),
                        aal: "aal1".into(),
                        methods: vec![],
                        tenant_id: Some(tenant),
                        role_ids: vec![],
                        authenticated_at: None,
                    });
                    req.extensions_mut()
                        .insert(sanvi_http::middleware::RequestId(uuid::Uuid::nil()));
                    next.call(req).await
                },
            ))
            .route(
                "/api/v1/tenant/catalog",
                web::get().to(|| async { HttpResponse::Ok().finish() }),
            ),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/catalog")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["instance"], "/api/v1/tenant/catalog");
    assert_eq!(body["trace_id"], uuid::Uuid::nil().to_string());
    assert_eq!(body["dimension"], "api_requests");
    assert_eq!(body["used"], 8);
    assert_eq!(body["limit"], 7);
    assert_eq!(
        body["upgrade_path"],
        "/api/v1/tenant/billing/checkout-session?plan=professional"
    );
}

#[tokio::test]
async fn quota_read_projection_repairs_stale_and_corrupt_cache_values() {
    let (_db, tx, tenant) = fixture().await;
    let client = redis::Client::open(sanvi_testsupport::redis_url().await).unwrap();
    let mut redis = redis::aio::ConnectionManager::new(client).await.unwrap();
    let namespace = sanvi_testsupport::test_stream("quota-read-repair");
    let quota = QuotaService::new(tx, Arc::new(SmallPlan), Some(redis.clone()))
        .with_cache_namespace(namespace.clone());
    quota
        .record(tenant, QuotaDimension::ApiRequests, 6, false)
        .await
        .unwrap();
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{namespace}:*"))
        .query_async(&mut redis)
        .await
        .unwrap();
    let key = keys
        .iter()
        .find(|key| key.contains(":api_requests:"))
        .unwrap();
    for version in ["0", "1"] {
        let _: i64 = redis::cmd("HSET")
            .arg(key)
            .arg("version")
            .arg(version)
            .arg("used")
            .arg("999")
            .query_async(&mut redis)
            .await
            .unwrap();
        assert_eq!(
            quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::ApiRequests].used,
            6
        );
        let repaired: String = redis::cmd("HGET")
            .arg(key)
            .arg("used")
            .query_async(&mut redis)
            .await
            .unwrap();
        assert_eq!(repaired, "6");
    }
}

#[tokio::test]
async fn quota_plan_tracks_live_subscription_and_downgrades_expired_grace() {
    use sanvi_http::quotas::PgQuotaPlanProvider;
    let (db, tx, tenant) = fixture().await;
    let provider = PgQuotaPlanProvider::new(tx);
    assert_eq!(provider.plan_for(tenant).await.unwrap().tier, "starter");
    let admin = db.admin_pool().await;
    sqlx::query(
        "INSERT INTO billing.subscriptions (id,tenant_id,plan_id,source,status)
        VALUES (510,500,2,'override','active')",
    )
    .execute(&admin)
    .await
    .unwrap();
    assert_eq!(
        provider.plan_for(tenant).await.unwrap().tier,
        "professional"
    );
    sqlx::query("UPDATE billing.subscriptions SET collection_state='grace_expired',collection_started_at=now() WHERE tenant_id=500")
        .execute(&admin).await.unwrap();
    assert_eq!(provider.plan_for(tenant).await.unwrap().tier, "starter");
}

#[tokio::test]
async fn quota_source_triggers_serialize_concurrent_domain_creates() {
    let (db, tx, tenant) = fixture().await;
    let quota = QuotaService::new(tx.clone(), Arc::new(SmallPlan), None);
    quota.configure_enforcement(tenant, true).await.unwrap();
    let mut tasks = Vec::new();
    for id in 600_i64..612 {
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            let mut write = tx.begin_for(500).await.unwrap();
            let result = sqlx::query(
                "INSERT INTO domains.custom_domains
                (id,tenant_id,hostname,normalized_hostname,kind,role,status)
                VALUES ($1,500,$2,$2,'connected','alias','pending_setup')",
            )
            .bind(id)
            .bind(format!("{id}.quota.test"))
            .execute(&mut **write.tx_mut().as_inner())
            .await;
            match result {
                Ok(_) => {
                    write.commit().await.unwrap();
                    true
                }
                Err(error) => {
                    assert!(
                        error
                            .to_string()
                            .contains("quota_exceeded|custom_domains|2|1|")
                    );
                    write.rollback().await.unwrap();
                    false
                }
            }
        }));
    }
    let mut accepted = 0;
    for task in tasks {
        if task.await.unwrap() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM domains.custom_domains")
        .fetch_one(&db.admin_pool().await)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::CustomDomains].used,
        1
    );
}

#[tokio::test]
async fn quota_month_rollover_keeps_resource_gauges_and_excludes_old_metering() {
    use chrono::{Datelike, Months, Utc};
    let (_db, tx, tenant) = fixture().await;
    let quota = QuotaService::new(tx.clone(), Arc::new(SmallPlan), None);
    quota
        .set(tenant, QuotaDimension::StorageBytes, 512, false)
        .await
        .unwrap();
    let previous_month = Utc::now()
        .date_naive()
        .with_day(1)
        .unwrap()
        .checked_sub_months(Months::new(1))
        .unwrap();
    let mut write = tx.begin_for(500).await.unwrap();
    sqlx::query(
        "INSERT INTO platform.quota_counters (tenant_id,dimension,period_start,used)
        VALUES (500,'api_requests',$1,999)",
    )
    .bind(previous_month)
    .execute(&mut **write.tx_mut().as_inner())
    .await
    .unwrap();
    write.commit().await.unwrap();
    let usage = quota.usage(tenant).await.unwrap();
    assert_eq!(usage.dimensions[&QuotaDimension::ApiRequests].used, 0);
    assert_eq!(usage.dimensions[&QuotaDimension::StorageBytes].used, 512);
}
#[async_trait]
impl QuotaPlanProvider for SmallPlan {
    async fn plan_for(&self, _: TenantId) -> Result<QuotaPlan, QuotaError> {
        let mut plan = QuotaPlan::for_tier("starter")?;
        plan.limits.insert(QuotaDimension::ApiRequests, 7);
        Ok(plan)
    }
}
async fn fixture() -> (sanvi_testsupport::TestDb, TxManager, TenantId) {
    let db = sanvi_testsupport::test_db().await;
    sqlx::query("INSERT INTO tenancy.tenants (id,slug,display_name,status,region) VALUES (500,'quota-test','Quota test','active','jp')")
        .execute(&db.admin_pool().await).await.unwrap();
    let tx = TxManager::new(db.app_pool(), sanvi_testsupport::test_ids());
    (db, tx, TenantId::try_from(500_i64).unwrap())
}

#[tokio::test]
async fn quota_survives_worker_restart_and_redis_key_flush() {
    let (_db, tx, tenant) = fixture().await;
    let client = redis::Client::open(sanvi_testsupport::redis_url().await).unwrap();
    let mut redis = redis::aio::ConnectionManager::new(client).await.unwrap();
    let namespace = sanvi_testsupport::test_stream("quota-restart");
    let quota = QuotaService::new(tx.clone(), Arc::new(SmallPlan), Some(redis.clone()))
        .with_cache_namespace(namespace.clone());
    quota
        .record(tenant, QuotaDimension::ApiRequests, 6, false)
        .await
        .unwrap();
    let restarted = QuotaService::new(tx, Arc::new(SmallPlan), Some(redis.clone()))
        .with_cache_namespace(namespace.clone());
    assert_eq!(
        restarted.usage(tenant).await.unwrap().dimensions[&QuotaDimension::ApiRequests].used,
        6
    );
    // Isolated deletion models a flush without harming other shared tests.
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{namespace}:*"))
        .query_async(&mut redis)
        .await
        .unwrap();
    assert!(!keys.is_empty());
    let _: i64 = redis::cmd("DEL")
        .arg(keys)
        .query_async(&mut redis)
        .await
        .unwrap();
    quota
        .record(tenant, QuotaDimension::ApiRequests, 2, false)
        .await
        .unwrap();
    assert_eq!(
        restarted.usage(tenant).await.unwrap().dimensions[&QuotaDimension::ApiRequests].used,
        8
    );
    match restarted
        .enforce(tenant, QuotaDimension::ApiRequests)
        .await
        .unwrap_err()
    {
        QuotaError::Exceeded {
            used,
            limit,
            upgrade_path,
            ..
        } => {
            assert_eq!((used, limit), (8, 7));
            assert!(upgrade_path.contains("professional"));
        }
        error => panic!("unexpected error: {error}"),
    }
}

#[tokio::test]
async fn quota_concurrent_reservations_never_oversubscribe() {
    let (_db, tx, tenant) = fixture().await;
    let quota = Arc::new(QuotaService::new(tx, Arc::new(SmallPlan), None));
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let quota = quota.clone();
        tasks.push(tokio::spawn(async move {
            quota
                .record(tenant, QuotaDimension::ApiRequests, 1, true)
                .await
        }));
    }
    let mut accepted = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => accepted += 1,
            Err(QuotaError::Exceeded { .. }) => (),
            Err(error) => panic!("unexpected error: {error}"),
        }
    }
    assert_eq!(accepted, 7);
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::ApiRequests].used,
        7
    );
    quota
        .record(tenant, QuotaDimension::ApiRequests, 1, false)
        .await
        .unwrap();
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::ApiRequests].used,
        8
    );
}

#[tokio::test]
async fn quota_gauges_can_be_released_and_invalid_deltas_do_not_mutate() {
    let (_db, tx, tenant) = fixture().await;
    let quota = QuotaService::new(tx, Arc::new(SmallPlan), None);
    quota
        .set(tenant, QuotaDimension::Seats, 3, false)
        .await
        .unwrap();
    assert_eq!(
        quota
            .record(tenant, QuotaDimension::Seats, -1, false)
            .await
            .unwrap()
            .used,
        2
    );
    assert!(
        quota
            .record(tenant, QuotaDimension::Seats, -3, false)
            .await
            .is_err()
    );
    assert!(
        quota
            .record(tenant, QuotaDimension::ApiRequests, -1, false)
            .await
            .is_err()
    );
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::Seats].used,
        2
    );
}

#[tokio::test]
async fn quota_producer_transaction_counts_enforces_and_rolls_back_together() {
    let (db, tx, tenant) = fixture().await;
    let quota = QuotaService::new(tx.clone(), Arc::new(SmallPlan), None);
    quota.configure_enforcement(tenant, true).await.unwrap();
    let insert="INSERT INTO domains.custom_domains (id,tenant_id,hostname,normalized_hostname,kind,role,status)
        VALUES ($1,500,$2,$2,'connected','alias','pending_setup')";
    let mut first = tx.begin_for(500).await.unwrap();
    sqlx::query(insert)
        .bind(501_i64)
        .bind("one.quota.test")
        .execute(&mut **first.tx_mut().as_inner())
        .await
        .unwrap();
    first.commit().await.unwrap();
    let mut second = tx.begin_for(500).await.unwrap();
    let error = sqlx::query(insert)
        .bind(502_i64)
        .bind("two.quota.test")
        .execute(&mut **second.tx_mut().as_inner())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("quota_exceeded|custom_domains|2|1|")
    );
    second.rollback().await.unwrap();
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::CustomDomains].used,
        1
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM domains.custom_domains")
        .fetch_one(&db.admin_pool().await)
        .await
        .unwrap();
    assert_eq!(count, 1);
    quota.configure_enforcement(tenant, false).await.unwrap();
    let mut third = tx.begin_for(500).await.unwrap();
    sqlx::query(insert)
        .bind(503_i64)
        .bind("three.quota.test")
        .execute(&mut **third.tx_mut().as_inner())
        .await
        .unwrap();
    third.commit().await.unwrap();
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::CustomDomains].used,
        2
    );
    quota.configure_enforcement(tenant, true).await.unwrap();
    let mut fourth = tx.begin_for(500).await.unwrap();
    sqlx::query("UPDATE domains.custom_domains SET status='verifying' WHERE id=501")
        .execute(&mut **fourth.tx_mut().as_inner())
        .await
        .unwrap();
    sqlx::query("UPDATE domains.custom_domains SET status='removed' WHERE id=503")
        .execute(&mut **fourth.tx_mut().as_inner())
        .await
        .unwrap();
    fourth.commit().await.unwrap();
    assert_eq!(
        quota.usage(tenant).await.unwrap().dimensions[&QuotaDimension::CustomDomains].used,
        1
    );
}
