//! Entitlement integration tests (testcontainers Postgres): an override
//! flips the gate for one tenant only, resolution composes defaults, and
//! quota consumption is atomic at the boundary (no oversell under 100
//! parallel calls).

use std::sync::Arc;

use sanvi_access::{
    adapters::{
        NoopEntitlementCache, SqlxEntitlementRepository, SqlxFeatureRepository,
        SqlxQuotaUsageStore, SystemClock,
    },
    application::entitlements::EntitlementPolicy,
    application::{
        EntitlementFeatureGate, EntitlementResolver, ExpireEntitlements, GrantEntitlement,
        GrantEntitlementCommand, SeedFeatures,
    },
    domain::{
        AccessError, ApprovalGate, Clock, EntitlementCache, EntitlementRepository, FeatureGate,
        FeatureKey, FeatureRepository, QuotaUsageStore,
    },
};
use sanvi_audit::{adapters::SqlxAuditWriter, domain::AuditWriter};
use sanvi_persistence::{DatabaseConfig, Migrator, TxManager, build_pool};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

fn migrations_dir() -> String {
    format!("{}/../../../migrations", env!("CARGO_MANIFEST_DIR"))
}

async fn prepare() -> (
    sanvi_persistence::PgPool,
    sanvi_persistence::PgPool,
    testcontainers::ContainerAsync<Postgres>,
) {
    let node = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres");
    let port = node.get_host_port_ipv4(5432).await.expect("postgres port");
    let admin = build_pool(&DatabaseConfig {
        host: "localhost".to_string(),
        port,
        name: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("admin pool");
    for statement in [
        "CREATE ROLE sanvi_migrator LOGIN PASSWORD 'migrator' NOSUPERUSER NOCREATEDB NOCREATEROLE",
        "CREATE ROLE sanvi_app LOGIN PASSWORD 'app' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS",
        "CREATE DATABASE sanvi_test OWNER sanvi_migrator",
    ] {
        sqlx::query(statement)
            .execute(&admin)
            .await
            .expect("role/database setup");
    }
    let migrator = build_pool(&DatabaseConfig {
        host: "localhost".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_migrator".to_string(),
        password: "migrator".to_string(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("migrator pool");
    Migrator::new(migrations_dir())
        .run(&migrator)
        .await
        .expect("migrations up");
    let app = build_pool(&DatabaseConfig {
        host: "localhost".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_app".to_string(),
        password: "app".to_string(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("app pool");
    (app, migrator, node)
}

/// Insert a tenant row directly (the entitlement FK needs one).
async fn insert_tenant(owner: &sanvi_persistence::PgPool, id: Uuid, slug: &str) {
    sqlx::query(
        "INSERT INTO tenancy.tenants
           (id, slug, display_name, status, region, default_locale,
            created_at, updated_at)
         VALUES ($1, $2, $3, 'active', 'us', 'en', now(), now())",
    )
    .bind(id)
    .bind(slug)
    .bind(format!("Tenant {slug}"))
    .execute(owner)
    .await
    .expect("tenant row");
}

struct Wired {
    gate: Arc<dyn FeatureGate>,
    grant: Arc<GrantEntitlement>,
    cache: Arc<dyn EntitlementCache>,
}

/// A four-eyes gate that never requires approval (these tests exercise the
/// direct grant path).
struct NoopApproval;

#[async_trait::async_trait]
impl ApprovalGate for NoopApproval {
    async fn request_if_required(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _requested_by: Uuid,
        _action_type: &str,
        _payload: serde_json::Value,
    ) -> Result<sanvi_access::domain::ApprovalOutcome, AccessError> {
        Ok(sanvi_access::domain::ApprovalOutcome::NotRequired)
    }
}

fn noop_approval() -> Arc<dyn ApprovalGate> {
    Arc::new(NoopApproval)
}

async fn wire(pool: sanvi_persistence::PgPool) -> Wired {
    let tx_manager = TxManager::new(pool.clone());
    let features: Arc<dyn FeatureRepository> = Arc::new(SqlxFeatureRepository::new());
    let entitlements: Arc<dyn EntitlementRepository> = Arc::new(SqlxEntitlementRepository::new());
    let quota: Arc<dyn QuotaUsageStore> = Arc::new(SqlxQuotaUsageStore::new());
    let events: Arc<dyn sanvi_access::domain::EventPublisher> =
        Arc::new(sanvi_access::adapters::OutboxEventPublisher::new());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cache: Arc<dyn EntitlementCache> = Arc::new(NoopEntitlementCache);
    let audit: Arc<dyn AuditWriter> = Arc::new(SqlxAuditWriter::new());

    // Seed the catalog first (idempotent).
    let seed = SeedFeatures::new(tx_manager.clone(), features.clone());
    seed.execute().await.expect("seed features");

    let resolver = Arc::new(EntitlementResolver::new(
        tx_manager.clone(),
        features.clone(),
        entitlements.clone(),
        cache.clone(),
        clock.clone(),
        300,
    ));
    let gate: Arc<dyn FeatureGate> = Arc::new(EntitlementFeatureGate::new(
        resolver,
        tx_manager.clone(),
        quota,
        events.clone(),
        clock.clone(),
    ));
    let grant = Arc::new(GrantEntitlement::new(
        tx_manager.clone(),
        features,
        entitlements,
        events,
        clock,
        audit,
        noop_approval(),
        EntitlementPolicy::default(),
    ));
    Wired { gate, grant, cache }
}

#[tokio::test]
async fn an_override_flips_the_gate_for_one_tenant_only() {
    let (app, migrator, _node) = prepare().await;
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    insert_tenant(&migrator, tenant_a, "acme").await;
    insert_tenant(&migrator, tenant_b, "beta").await;

    let wired = wire(app.clone()).await;
    let feature = FeatureKey::parse("advertising.google_ads").unwrap();

    // Default: disabled everywhere.
    assert!(!wired.gate.is_enabled(tenant_a, &feature).await.unwrap());
    assert!(!wired.gate.is_enabled(tenant_b, &feature).await.unwrap());

    // Grant to A only.
    let grant_result = wired
        .grant
        .execute(
            Uuid::now_v7(),
            None,
            GrantEntitlementCommand {
                tenant_id: tenant_a,
                feature_key: feature.clone(),
                enabled: true,
                limit: None,
                expires_at: None,
                reason: Some("acceptance test".to_string()),
                skip_approval: false,
            },
        )
        .await;
    if let Err(err) = &grant_result {
        eprintln!("grant failed: {err:?}");
    }
    grant_result.expect("grant override");

    // Noop cache in this test: resolution hits the database directly, so
    // the flip is immediately visible — no invalidation window at all.
    assert!(wired.gate.is_enabled(tenant_a, &feature).await.unwrap());
    assert!(!wired.gate.is_enabled(tenant_b, &feature).await.unwrap());

    // The override row is visible to the operator listing.
    let overrides = sanvi_access::application::ListTenantOverrides::new(
        TxManager::new(app),
        Arc::new(SqlxEntitlementRepository::new()),
    )
    .execute(tenant_a)
    .await
    .unwrap();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].feature(), &feature);
    assert!(overrides[0].enabled());
}

#[tokio::test]
async fn quota_consumption_never_oversells_under_concurrency() {
    let (app, migrator, _node) = prepare().await;
    let tenant = Uuid::now_v7();
    insert_tenant(&migrator, tenant, "quota").await;

    let wired = wire(app.clone()).await;
    let feature = FeatureKey::parse("tenancy.member_limit").unwrap();

    // Set the quota via an override with limit 10.
    wired
        .grant
        .execute(
            Uuid::now_v7(),
            None,
            GrantEntitlementCommand {
                tenant_id: tenant,
                feature_key: feature.clone(),
                enabled: true,
                limit: Some(10),
                expires_at: None,
                reason: Some("quota test".to_string()),
                skip_approval: false,
            },
        )
        .await
        .unwrap();

    // 100 parallel consumers × 1 unit against a limit of 10: exactly 10
    // succeed, 90 get QuotaExceeded — never 11.
    let mut handles = Vec::with_capacity(100);
    for _ in 0..100 {
        let gate = wired.gate.clone();
        let feature = feature.clone();
        handles.push(tokio::spawn(async move {
            gate.consume(tenant, &feature, 1).await
        }));
    }
    let mut succeeded = 0;
    let mut exceeded = 0;
    for handle in handles {
        match handle.await.unwrap() {
            Ok(()) => succeeded += 1,
            Err(AccessError::QuotaExceeded { .. }) => exceeded += 1,
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
    assert_eq!(succeeded, 10, "exactly the limit may succeed");
    assert_eq!(exceeded, 90);

    // Remaining is exhausted.
    assert_eq!(
        wired.gate.remaining(tenant, &feature).await.unwrap(),
        Some(0)
    );

    // QuotaExceeded events were recorded in the outbox for the failed
    // attempts.
    let breaches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE event_type = 'access.quota_exceeded'",
    )
    .fetch_one(&app)
    .await
    .unwrap();
    assert_eq!(breaches, 90);
}

#[tokio::test]
async fn expired_overrides_fall_back_to_defaults_and_sweep() {
    let (app, migrator, _node) = prepare().await;
    let tenant = Uuid::now_v7();
    insert_tenant(&migrator, tenant, "expiry").await;

    let tx_manager = TxManager::new(app.clone());
    let wired = wire(app.clone()).await;
    let feature = FeatureKey::parse("advertising.google_ads").unwrap();

    // A grant that expires one second from now.
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(1);
    wired
        .grant
        .execute(
            Uuid::now_v7(),
            None,
            GrantEntitlementCommand {
                tenant_id: tenant,
                feature_key: feature.clone(),
                enabled: true,
                limit: None,
                expires_at: Some(expires_at),
                reason: None,
                skip_approval: false,
            },
        )
        .await
        .unwrap();
    assert!(wired.gate.is_enabled(tenant, &feature).await.unwrap());

    // After expiry, resolution ignores the row (default: disabled).
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(!wired.gate.is_enabled(tenant, &feature).await.unwrap());

    // The sweep deletes the expired row and publishes the event.
    let sweep = ExpireEntitlements::new(
        tx_manager,
        Arc::new(SqlxEntitlementRepository::new()),
        wired.cache.clone(),
        Arc::new(sanvi_access::adapters::OutboxEventPublisher::new()),
        Arc::new(SystemClock),
    );
    assert_eq!(sweep.execute(10).await.unwrap(), 1);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE event_type = 'access.entitlement_expired'",
    )
    .fetch_one(&app)
    .await
    .unwrap();
    assert_eq!(events, 1);
}
