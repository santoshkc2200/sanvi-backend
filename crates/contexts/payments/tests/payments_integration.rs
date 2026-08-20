//! The payments context's integration tests (sub-phase 09.0).
//!
//! Real Postgres in containers, the `sanvi_app` role (NOBYPASSRLS), full
//! migrations — the same recipe the domains/billing suites use. Covers the
//! three things this slice must prove: the migration round-trips on a
//! seeded database, RLS isolates tenants (and deliberately does not apply
//! to the platform-scoped webhook table), and the catalog route enforces
//! permission + entitlement before it answers.

use std::sync::Arc;

use actix_web::{App, http::StatusCode, test, web};
use chrono::Utc;
use uuid::Uuid;

use sanvi_http::{FeatureGate, PermissionGate, ResourceRef, Subject, tenancy::TenantContext};
use sanvi_payments::adapters::http::PaymentsState;
use sanvi_payments::adapters::repos::SqlxProviderConnectionRepository;
use sanvi_payments::adapters::{EmptyProviderCatalog, PaymentsMetrics};
use sanvi_payments::application::ListProviders;
use sanvi_payments::config::PaymentsConfig;
use sanvi_payments::domain::provider::{ConnectionStatus, ProviderKind};
use sanvi_payments::domain::{Money, ProviderConnection, ProviderConnectionRepository};
use sanvi_payments::routes::payments_tenant_scope_routes;
use sanvi_persistence::{DatabaseConfig, Migrator, TxManager, build_pool};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

fn migrations_dir() -> std::path::PathBuf {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join("../../../migrations"))
        .unwrap_or_else(|_| std::path::PathBuf::from("migrations"))
}

/// The migration this slice ships (up/down round-trip target).
const PAYMENTS_MIGRATION: i64 = 20_260_823_000_000;

async fn start_postgres() -> (u16, testcontainers::ContainerAsync<Postgres>) {
    let node = Postgres::default().start().await.unwrap();
    let port = node.get_host_port_ipv4(5432).await.unwrap();
    let admin = sqlx::PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
    ))
    .await
    .unwrap();
    sqlx::query("CREATE ROLE sanvi_migrator LOGIN PASSWORD 'mig'")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE ROLE sanvi_app LOGIN PASSWORD 'app' NOBYPASSRLS")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE sanvi_test OWNER sanvi_migrator")
        .execute(&admin)
        .await
        .unwrap();
    (port, node)
}

fn db_config(port: u16, user: &str, password: &str) -> DatabaseConfig {
    DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: user.to_string(),
        password: password.to_string(),
        ..DatabaseConfig::default()
    }
}

async fn migrate(port: u16) -> sqlx::PgPool {
    let config = db_config(port, "sanvi_migrator", "mig");
    let url = format!(
        "postgres://{}:{}@127.0.0.1:{}/{}",
        config.user, config.password, port, config.name
    );
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    Migrator::new(migrations_dir()).run(&pool).await.unwrap();
    pool
}

async fn app_pool(port: u16) -> sanvi_persistence::PgPool {
    build_pool(&db_config(port, "sanvi_app", "app"))
        .await
        .unwrap()
}

async fn seed_tenant(pool: &sqlx::PgPool, slug: &str) -> Uuid {
    let tenant_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, $2, $3, 'active', 'jp', 'en')",
    )
    .bind(tenant_id)
    .bind(slug)
    .bind(format!("{slug} corp"))
    .execute(pool)
    .await
    .unwrap();
    tenant_id
}

// ---------------------------------------------------------------------------
// Migration up/down round-trip on a seeded database.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_round_trips_on_a_seeded_database() {
    let (port, _node) = start_postgres().await;
    let migrator_pool = migrate(port).await;
    let pool = app_pool(port).await;

    let tenant_id = seed_tenant(&pool, "round-trip").await;
    let connection = ProviderConnection::pending(
        Uuid::now_v7(),
        tenant_id,
        ProviderKind::StripeConnect,
        format!("acct_{}", Uuid::now_v7().simple()),
        Utc::now(),
    )
    .unwrap();

    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone());
    {
        let mut scope = tx.begin_for(tenant_id).await.unwrap();
        repo.insert(&mut scope, &connection).await.unwrap();
        scope.commit().await.unwrap();
    }

    // Down: the schema (and the seeded row) disappear; everything before
    // this migration is untouched.
    let migrator = Migrator::new(migrations_dir());
    migrator
        .undo(&migrator_pool, PAYMENTS_MIGRATION)
        .await
        .unwrap();
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'payments')",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(!exists, "the payments schema must be gone after undo");
    let tenants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenancy.tenants")
        .fetch_one(&migrator_pool)
        .await
        .unwrap();
    assert_eq!(tenants, 1, "earlier migrations survive the undo");

    // …and back up again — the rollback rehearsal path is reversible too.
    migrator.run(&migrator_pool).await.unwrap();
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'payments')",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(exists);
}

// ---------------------------------------------------------------------------
// RLS: tenant isolation on the tenant-scoped tables.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tenant_a_cannot_read_tenant_bs_connections() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;

    let tenant_a = seed_tenant(&pool, "alpha").await;
    let tenant_b = seed_tenant(&pool, "beta").await;

    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone());
    let connection = ProviderConnection::pending(
        Uuid::now_v7(),
        tenant_a,
        ProviderKind::StripeConnect,
        "acct_alpha".to_string(),
        Utc::now(),
    )
    .unwrap();
    {
        let mut scope = tx.begin_for(tenant_a).await.unwrap();
        repo.insert(&mut scope, &connection).await.unwrap();
        scope.commit().await.unwrap();
    }

    // Through the repo: A sees its connection, B sees nothing.
    {
        let mut scope = tx.begin_for(tenant_a).await.unwrap();
        assert_eq!(repo.list(&mut scope, tenant_a).await.unwrap().len(), 1);
        scope.commit().await.unwrap();
    }
    {
        let mut scope = tx.begin_for(tenant_b).await.unwrap();
        assert!(repo.list(&mut scope, tenant_b).await.unwrap().is_empty());
        assert!(
            repo.find_active(&mut scope, tenant_b, "stripe_connect")
                .await
                .unwrap()
                .is_none()
        );
        scope.commit().await.unwrap();
    }

    // Directly in SQL: the same scoped transaction A uses sees A's row…
    {
        let mut scope = tx.begin_for(tenant_a).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments.provider_connections")
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
        assert_eq!(count, 1);
        scope.commit().await.unwrap();
    }
    // …and B's scoped transaction sees none of it, with no predicate in
    // the query — RLS is the enforcement, not the WHERE clause.
    {
        let mut scope = tx.begin_for(tenant_b).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments.provider_connections")
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
        assert_eq!(count, 0);
        scope.commit().await.unwrap();
    }
}

#[tokio::test]
async fn one_active_connection_per_provider_but_history_is_kept() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "single").await;

    // Two live rows for the same provider collide (partial unique index)…
    let first = ProviderConnection::pending(
        Uuid::now_v7(),
        tenant_id,
        ProviderKind::StripeConnect,
        "acct_one".to_string(),
        Utc::now(),
    )
    .unwrap();
    let second = ProviderConnection::pending(
        Uuid::now_v7(),
        tenant_id,
        ProviderKind::StripeConnect,
        "acct_two".to_string(),
        Utc::now(),
    )
    .unwrap();
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone());
    {
        let mut scope = tx.begin_for(tenant_id).await.unwrap();
        repo.insert(&mut scope, &first).await.unwrap();
        scope.commit().await.unwrap();
    }
    {
        let mut scope = tx.begin_for(tenant_id).await.unwrap();
        let err = repo.insert(&mut scope, &second).await.unwrap_err();
        assert!(matches!(err, sanvi_payments::PaymentsError::Storage(_)));
        let _ = scope.rollback().await;
    }

    // …but a disconnected row no longer counts against the index, and the
    // history stays for the statutory period.
    sqlx::query(
        "UPDATE payments.provider_connections
         SET disconnected_at = now(), status = 'disconnected' WHERE id = $1",
    )
    .bind(first.id)
    .execute(&pool)
    .await
    .unwrap();
    {
        let mut scope = tx.begin_for(tenant_id).await.unwrap();
        repo.insert(&mut scope, &second).await.unwrap();
        scope.commit().await.unwrap();
    }
    {
        let mut scope = tx.begin_for(tenant_id).await.unwrap();
        let history = repo.list(&mut scope, tenant_id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].status, ConnectionStatus::Pending);
        assert_eq!(history[1].status, ConnectionStatus::Disconnected);
        scope.commit().await.unwrap();
    }
}

#[tokio::test]
async fn webhook_events_are_platform_scoped_not_tenant_readable() {
    let (port, _node) = start_postgres().await;

    // The four tenant-scoped tables carry the tenant_isolation policy;
    // webhook_events deliberately does not (Connect events arrive before
    // the tenant is known and map to one only through
    // external_account_id — the table is touched solely through
    // platform-scope transactions).
    let config = db_config(port, "sanvi_migrator", "mig");
    let url = format!(
        "postgres://{}:{}@127.0.0.1:{}/{}",
        config.user, config.password, port, config.name
    );
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    Migrator::new(migrations_dir()).run(&admin).await.unwrap();

    for table in ["provider_connections", "checkouts", "payments", "refunds"] {
        let policies: Vec<(String,)> = sqlx::query_as(
            "SELECT policyname FROM pg_policies WHERE schemaname = 'payments' AND tablename = $1",
        )
        .bind(table)
        .fetch_all(&admin)
        .await
        .unwrap();
        assert!(
            policies
                .iter()
                .any(|(policy,)| policy == "tenant_isolation"),
            "{table} must carry the tenant_isolation policy"
        );
    }

    let no_rls: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'payments' AND rowsecurity = false",
    )
    .fetch_all(&admin)
    .await
    .unwrap();
    assert_eq!(
        no_rls,
        vec![("webhook_events".to_string(),)],
        "webhook_events is platform-scoped: no RLS, joined to tenants only through external_account_id"
    );
}

// ---------------------------------------------------------------------------
// The catalog route: permission + entitlement gating.
// ---------------------------------------------------------------------------

struct FakePermissionGate {
    allowed: bool,
}

impl PermissionGate for FakePermissionGate {
    fn check<'a>(
        &'a self,
        _subject: &'a Subject,
        _permission: &'a str,
        _resource: &'a ResourceRef,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<sanvi_http::Decision, sanvi_http::ApiError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Ok(if self.allowed {
                sanvi_http::Decision::Allowed
            } else {
                sanvi_http::Decision::Denied
            })
        })
    }
}

struct FakeFeatureGate {
    enabled: bool,
}

impl FeatureGate for FakeFeatureGate {
    fn check<'a>(
        &'a self,
        _tenant_id: Uuid,
        _feature: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<bool, sanvi_http::ApiError>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(self.enabled) })
    }
}

fn payments_state() -> PaymentsState {
    let registry = prometheus::Registry::new();
    PaymentsState {
        config: PaymentsConfig::default(),
        metrics: Arc::new(PaymentsMetrics::new(&registry)),
        list_providers: Arc::new(ListProviders::new(Arc::new(EmptyProviderCatalog))),
    }
}

/// Insert the authn/tenancy middleware products directly (the extractors
/// only read request extensions — this is exactly what the real middleware
/// does one layer out).
async fn spawn_app(
    permission_allowed: bool,
    entitlement_enabled: bool,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse,
    Error = actix_web::Error,
> {
    let tenant_id = Uuid::now_v7();
    let subject = Subject {
        user_id: Uuid::now_v7(),
        session_id: "sess_test".to_string(),
        aal: "aal2".to_string(),
        methods: vec!["password".to_string()],
        tenant_id: Some(tenant_id),
        role_ids: vec![],
        authenticated_at: Some(Utc::now()),
    };
    let tenant = TenantContext {
        tenant_id,
        slug: "test".to_string(),
        display_name: "Test".to_string(),
        status: sanvi_http::tenancy::TenantRuntimeStatus::Active,
        region: "jp".to_string(),
        default_locale: "en".to_string(),
        resolution_source: sanvi_http::tenancy::ResolutionSource::Subdomain,
    };

    let permission_gate: Arc<dyn PermissionGate> = Arc::new(FakePermissionGate {
        allowed: permission_allowed,
    });
    let feature_gate: Arc<dyn FeatureGate> = Arc::new(FakeFeatureGate {
        enabled: entitlement_enabled,
    });

    test::init_service(
        App::new()
            .app_data(web::Data::new(payments_state()))
            .app_data(web::Data::new(permission_gate))
            .app_data(web::Data::new(feature_gate))
            .wrap(actix_web::middleware::from_fn(
                move |req: actix_web::dev::ServiceRequest,
                      srv: actix_web::middleware::Next| {
                    let subject = subject.clone();
                    let tenant = tenant.clone();
                    async move {
                        req.extensions_mut().insert(subject);
                        req.extensions_mut().insert(tenant);
                        srv.call(req).await
                    }
                },
            ))
            .service(web::scope("/api/v1/tenant").configure(payments_tenant_scope_routes)),
    )
    .await
}

#[tokio::test]
async fn the_catalog_route_requires_the_entitlement() {
    let app = spawn_app(true, false).await; // permission OK, entitlement off
    let req = test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = test::read_body_json(res).await;
    // 403 with the feature named — never a bare 404 — so the frontend can
    // render its upgrade prompt.
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("payments.stripe_connect")
    );
}

#[tokio::test]
async fn the_catalog_route_requires_payments_read() {
    let app = spawn_app(false, true).await; // entitlement OK, permission denied
    let req = test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert!(body["detail"].as_str().unwrap().contains("payments.read"));
}

#[tokio::test]
async fn with_permission_and_entitlement_the_catalog_answers_empty() {
    let app = spawn_app(true, true).await;
    let req = test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["providers"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// The domain money guard, exercised where later slices will call it.
// ---------------------------------------------------------------------------

#[test]
fn partial_amounts_larger_than_the_total_are_rejected_in_both_currencies() {
    let usd = Money::new(1999, "USD").unwrap();
    assert_eq!(usd.portion_of(999).unwrap().amount_minor, 999);
    assert!(usd.portion_of(2000).is_err());

    // JPY is zero-decimal: ¥2,000 IS the minor-unit integer 2000 — the
    // case that catches every hidden ÷100.
    let jpy = Money::new(2000, "JPY").unwrap();
    assert_eq!(jpy.portion_of(2000).unwrap().amount_minor, 2000);
    assert!(jpy.portion_of(2001).is_err());
}
