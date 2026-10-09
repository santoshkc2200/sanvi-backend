use sanvi_http::external_quotas::ExternalOutcome;
use sanvi_http::quota_middleware::QuotaRuntime;
use sanvi_http::quotas::{PgQuotaPlanProvider, QuotaDimension, QuotaService};
use sanvi_persistence::TxManager;
use std::sync::Arc;

#[tokio::test]
async fn external_accounting_migration_supports_durable_idempotency() {
    let db = sanvi_testsupport::test_db().await;
    let exists: bool = sqlx::query_scalar(
        "SELECT to_regclass('platform.external_quota_reservations') IS NOT NULL",
    )
    .fetch_one(&db.app_pool())
    .await
    .unwrap();
    assert!(
        exists,
        "external-service reservations must survive retries and process restart"
    );
}

async fn fixture() -> (
    sanvi_testsupport::TestDb,
    QuotaRuntime,
    sanvi_kernel::TenantId,
) {
    let db = sanvi_testsupport::test_db().await;
    sqlx::query("INSERT INTO tenancy.tenants(id,slug,display_name,status,region) VALUES(600,'external-quota','External quota','active','jp')")
        .execute(&db.admin_pool().await).await.unwrap();
    let tx = TxManager::new(db.app_pool(), sanvi_testsupport::test_ids());
    let runtime = QuotaRuntime {
        service: Arc::new(QuotaService::new(
            tx.clone(),
            Arc::new(PgQuotaPlanProvider::new(tx)),
            None,
        )),
        enabled: false,
    };
    (db, runtime, "600".parse().unwrap())
}

#[tokio::test]
async fn external_retries_restart_and_asset_deletion_count_once() {
    let (_db, runtime, tenant) = fixture().await;
    for _ in 0..3 {
        runtime
            .reserve_external(tenant, QuotaDimension::NotificationSends, "mail-key", 1)
            .await
            .unwrap();
        runtime
            .reserve_external(tenant, QuotaDimension::StorageBytes, "upload-key", 100)
            .await
            .unwrap();
    }
    runtime
        .bind_asset(tenant, "upload-key", "upstream-asset")
        .await
        .unwrap();
    let restarted = QuotaRuntime {
        service: runtime.service.clone(),
        enabled: false,
    };
    restarted
        .reserve_external(tenant, QuotaDimension::NotificationSends, "mail-key", 1)
        .await
        .unwrap();
    let usage = restarted.service.usage(tenant).await.unwrap();
    assert_eq!(usage.dimensions[&QuotaDimension::NotificationSends].used, 1);
    assert_eq!(usage.dimensions[&QuotaDimension::StorageBytes].used, 100);
    restarted
        .release_asset(tenant, "upstream-asset")
        .await
        .unwrap();
    restarted
        .release_asset(tenant, "upstream-asset")
        .await
        .unwrap();
    assert_eq!(
        restarted.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::StorageBytes]
            .used,
        0
    );
}

#[tokio::test]
async fn external_quota_breach_returns_upgrade_and_does_not_reserve() {
    let (_db, mut runtime, tenant) = fixture().await;
    runtime.enabled = true;
    let limit =
        runtime.service.plan_for(tenant).await.unwrap().limits[&QuotaDimension::StorageBytes];
    let error = runtime
        .reserve_external(
            tenant,
            QuotaDimension::StorageBytes,
            "too-large",
            limit as i64 + 1,
        )
        .await
        .unwrap_err();
    assert_eq!(error.status().as_u16(), 403);
    assert!(error.to_string().contains("checkout-session"));
    assert_eq!(
        runtime.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::StorageBytes].used,
        0
    );
}

#[tokio::test]
async fn definitive_rejections_refund_once_and_retire_the_key() {
    let (_db, runtime, tenant) = fixture().await;
    let ticket = runtime
        .reserve_external_payload(
            tenant,
            QuotaDimension::NotificationSends,
            "invalid-mail",
            1,
            b"invalid body",
        )
        .await
        .unwrap()
        .unwrap();
    runtime
        .finish_external(&ticket, ExternalOutcome::Rejected)
        .await
        .unwrap();
    runtime
        .finish_external(&ticket, ExternalOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!(
        runtime.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::NotificationSends]
            .used,
        0
    );
    let error = runtime
        .reserve_external_payload(
            tenant,
            QuotaDimension::NotificationSends,
            "invalid-mail",
            1,
            b"invalid body",
        )
        .await
        .unwrap_err();
    assert_eq!(error.status().as_u16(), 409);
}

#[tokio::test]
async fn concurrent_retries_cannot_refund_an_accepted_or_ambiguous_dispatch() {
    let (_db, runtime, tenant) = fixture().await;
    let a = runtime
        .reserve_external_payload(
            tenant,
            QuotaDimension::StorageBytes,
            "shared-upload",
            100,
            b"same payload",
        )
        .await
        .unwrap()
        .unwrap();
    let b = runtime
        .reserve_external_payload(
            tenant,
            QuotaDimension::StorageBytes,
            "shared-upload",
            100,
            b"same payload",
        )
        .await
        .unwrap()
        .unwrap();
    runtime
        .finish_external(&a, ExternalOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!(
        runtime.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::StorageBytes].used,
        100
    );
    runtime
        .finish_external(
            &b,
            ExternalOutcome::Accepted {
                asset_id: Some("accepted-asset".into()),
            },
        )
        .await
        .unwrap();
    runtime
        .finish_external(&a, ExternalOutcome::Rejected)
        .await
        .unwrap();
    let c = runtime
        .reserve_external_payload(
            tenant,
            QuotaDimension::StorageBytes,
            "shared-upload",
            100,
            b"same payload",
        )
        .await
        .unwrap()
        .unwrap();
    runtime
        .finish_external(&c, ExternalOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!(
        runtime.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::StorageBytes].used,
        100
    );
    runtime
        .release_asset(tenant, "accepted-asset")
        .await
        .unwrap();
    assert_eq!(
        runtime.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::StorageBytes].used,
        0
    );
    let d = runtime
        .reserve_external_payload(
            tenant,
            QuotaDimension::NotificationSends,
            "ambiguous",
            1,
            b"payload",
        )
        .await
        .unwrap()
        .unwrap();
    runtime
        .finish_external(&d, ExternalOutcome::Ambiguous)
        .await
        .unwrap();
    let e = runtime
        .reserve_external_payload(
            tenant,
            QuotaDimension::NotificationSends,
            "ambiguous",
            1,
            b"payload",
        )
        .await
        .unwrap()
        .unwrap();
    runtime
        .finish_external(&e, ExternalOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!(
        runtime.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::NotificationSends]
            .used,
        1
    );
}

#[tokio::test]
async fn same_key_and_size_with_different_payload_is_rejected_before_dispatch() {
    let (_db, runtime, tenant) = fixture().await;
    runtime
        .reserve_external_payload(tenant, QuotaDimension::StorageBytes, "key", 100, b"first")
        .await
        .unwrap();
    let error = runtime
        .reserve_external_payload(
            tenant,
            QuotaDimension::StorageBytes,
            "key",
            100,
            b"different",
        )
        .await
        .unwrap_err();
    assert_eq!(error.status().as_u16(), 400);
}

#[tokio::test]
async fn deleted_asset_keys_are_retired_in_both_shadow_and_enforced_modes() {
    let (_db, mut runtime, tenant) = fixture().await;
    runtime
        .reserve_external(tenant, QuotaDimension::StorageBytes, "deleted-key", 100)
        .await
        .unwrap();
    runtime
        .bind_asset(tenant, "deleted-key", "deleted-asset")
        .await
        .unwrap();
    runtime
        .release_asset(tenant, "deleted-asset")
        .await
        .unwrap();
    for enabled in [false, true] {
        runtime.enabled = enabled;
        assert_eq!(
            runtime
                .reserve_external(tenant, QuotaDimension::StorageBytes, "deleted-key", 100)
                .await
                .unwrap_err()
                .status()
                .as_u16(),
            409
        );
    }
}

#[tokio::test]
async fn external_quota_shadow_mode_allows_accounting_infrastructure_failure() {
    use async_trait::async_trait;
    use sanvi_http::quotas::{QuotaError, QuotaPlan, QuotaPlanProvider};
    struct Unavailable;
    #[async_trait]
    impl QuotaPlanProvider for Unavailable {
        async fn plan_for(&self, _: sanvi_kernel::TenantId) -> Result<QuotaPlan, QuotaError> {
            Err(QuotaError::Infrastructure(anyhow::anyhow!(
                "billing temporarily unavailable"
            )))
        }
    }
    let (db, _, tenant) = fixture().await;
    let manager = TxManager::new(db.app_pool(), sanvi_testsupport::test_ids());
    let mut runtime = QuotaRuntime {
        service: Arc::new(QuotaService::new(manager, Arc::new(Unavailable), None)),
        enabled: false,
    };
    assert!(
        runtime
            .reserve_external_payload(
                tenant,
                QuotaDimension::NotificationSends,
                "offline",
                1,
                b"payload"
            )
            .await
            .unwrap()
            .is_none()
    );
    runtime
        .reserve_external(tenant, QuotaDimension::StorageBytes, "offline-storage", 100)
        .await
        .unwrap();
    runtime.enabled = true;
    assert_eq!(
        runtime
            .reserve_external_payload(
                tenant,
                QuotaDimension::NotificationSends,
                "offline",
                1,
                b"payload"
            )
            .await
            .unwrap_err()
            .status()
            .as_u16(),
        503
    );
}

#[tokio::test]
async fn outstanding_attempts_are_bounded_and_shadow_bypass_retains_unknown_allocation() {
    let (_db, mut runtime, tenant) = fixture().await;
    runtime.enabled = true;
    let mut tickets = Vec::new();
    for _ in 0..128 {
        tickets.push(
            runtime
                .reserve_external_payload(
                    tenant,
                    QuotaDimension::NotificationSends,
                    "many-retries",
                    1,
                    b"payload",
                )
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(
        runtime
            .reserve_external_payload(
                tenant,
                QuotaDimension::NotificationSends,
                "many-retries",
                1,
                b"payload"
            )
            .await
            .unwrap_err()
            .status()
            .as_u16(),
        503
    );
    runtime.enabled = false;
    assert!(
        runtime
            .reserve_external_payload(
                tenant,
                QuotaDimension::NotificationSends,
                "many-retries",
                1,
                b"payload"
            )
            .await
            .unwrap()
            .is_none()
    );
    for ticket in tickets {
        runtime
            .finish_external(&ticket, ExternalOutcome::Rejected)
            .await
            .unwrap();
    }
    assert_eq!(
        runtime.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::NotificationSends]
            .used,
        1
    );
}
