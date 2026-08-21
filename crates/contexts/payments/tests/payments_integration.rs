//! The payments context's integration tests (sub-phases 09.0–09.2).
//!
//! Real Postgres in containers, the `sanvi_app` role (NOBYPASSRLS), full
//! migrations — the same recipe the domains/billing suites use. 09.0
//! covered the schema, RLS and the catalog route's gates; 09.1 added the
//! registry-derived catalog and the registry → port → repo → outbox
//! vertical; 09.2 adds the idempotent connection create (concurrency and
//! orphan adoption), the account-session use case's refusals, the route
//! gates on the new endpoints, and the audit trail.

use std::sync::Arc;

// `test` is aliased: `actix_web` re-exports both a `test` module (these
// helpers) and a `#[test]`-shadowing attribute macro at the same path, so a
// bare `use actix_web::test;` would silently turn the plain `#[test]`
// below into `#[actix_web::test]` and demand an async fn.
use actix_web::{App, HttpMessage, http::StatusCode, test as actix_test, web};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use sanvi_http::{FeatureGate, PermissionGate, ResourceRef, Subject, tenancy::TenantContext};
use sanvi_payments::adapters::http::PaymentsState;
use sanvi_payments::adapters::repos::SqlxProviderConnectionRepository;
use sanvi_payments::adapters::{FakeProvider, OutboxEventPublisher, PaymentsMetrics};
use sanvi_payments::application::{
    CreateAccountSession, ListProviders, ProviderRegistry, RegistryProviderCatalog,
    RuntimeEnvironment, StartConnectionCommand, StartProviderConnection,
};
use sanvi_payments::config::PaymentsConfig;
use sanvi_payments::domain::ports::{CreateAccountRequest, PaymentProviderPort};
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
    // `undo(pool, target)` reverts every migration with version > target,
    // i.e. target is the version to end up AT, not the one to revert — so
    // this slice's own migration needs `PAYMENTS_MIGRATION - 1` here, or it
    // is left applied instead of undone.
    migrator
        .undo(&migrator_pool, PAYMENTS_MIGRATION - 1)
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
        // 09.2: the partial unique index maps to the 409-shaped domain
        // error, not a raw storage failure.
        assert!(matches!(
            err,
            sanvi_payments::PaymentsError::ConnectionExists(_)
        ));
        let _ = scope.rollback().await;
    }

    // …but a disconnected row no longer counts against the index, and the
    // history stays for the statutory period. RLS requires the tenant
    // context (`sanvi.tenant_id`), so this has to run inside a scoped
    // transaction like every other write — a raw pool query silently
    // matches zero rows instead of erroring.
    {
        let mut scope = tx.begin_for(tenant_id).await.unwrap();
        sqlx::query(
            "UPDATE payments.provider_connections
             SET disconnected_at = now(), status = 'disconnected' WHERE id = $1",
        )
        .bind(first.id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }
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

fn payments_state(environment: RuntimeEnvironment) -> PaymentsState {
    let prometheus_registry = prometheus::Registry::new();
    let mut providers = ProviderRegistry::new();
    if environment == RuntimeEnvironment::NonProduction {
        providers.register(Arc::new(FakeProvider::new())).unwrap();
    }
    let providers = Arc::new(providers);
    let catalog = Arc::new(RegistryProviderCatalog::new(providers.clone(), environment));

    // A never-connected lazy pool: the route-gating tests below never
    // execute the connection use case, so there is nothing to dial.
    let lazy_pool = sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
        .expect("lazy pool construction is configuration only");
    let tx = TxManager::new(lazy_pool);
    let connections: Arc<dyn sanvi_payments::domain::ProviderConnectionRepository> =
        Arc::new(SqlxProviderConnectionRepository::new());
    let audit: Arc<dyn sanvi_audit::domain::AuditWriter> =
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new());

    PaymentsState {
        config: PaymentsConfig::default(),
        metrics: Arc::new(PaymentsMetrics::new(&prometheus_registry)),
        list_providers: Arc::new(ListProviders::new(catalog)),
        registry: providers.clone(),
        tx: tx.clone(),
        connections: connections.clone(),
        create_connection: Arc::new(StartProviderConnection::new(
            tx,
            providers.clone(),
            connections.clone(),
            Arc::new(OutboxEventPublisher::new()),
            audit,
        )),
        account_session: Arc::new(sanvi_payments::application::CreateAccountSession::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
            ),
            providers,
            connections,
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
        )),
        webhook_receiver: Arc::new(sanvi_payments::application::WebhookReceiver::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
            ),
            Arc::new(sanvi_payments::adapters::SqlxWebhookEventRepository::new()),
            Arc::new(FakeProvider::new()),
            vec!["127.0.0.1".to_string(), "::1".to_string()],
        )),
    }
}

/// Insert the authn/tenancy middleware products directly (the extractors
/// only read request extensions — this is exactly what the real middleware
/// does one layer out).
#[allow(clippy::too_many_arguments)]
async fn spawn_app(
    permission_allowed: bool,
    entitlement_enabled: bool,
    environment: RuntimeEnvironment,
    region: &str,
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
        region: region.to_string(),
        default_locale: "en".to_string(),
        resolution_source: sanvi_http::tenancy::ResolutionSource::Subdomain,
    };

    let permission_gate: Arc<dyn PermissionGate> = Arc::new(FakePermissionGate {
        allowed: permission_allowed,
    });
    let feature_gate: Arc<dyn FeatureGate> = Arc::new(FakeFeatureGate {
        enabled: entitlement_enabled,
    });

    actix_test::init_service(
        App::new()
            .app_data(web::Data::new(payments_state(environment)))
            .app_data(web::Data::new(permission_gate))
            .app_data(web::Data::new(feature_gate))
            .wrap(actix_web::middleware::from_fn(
                move |req: actix_web::dev::ServiceRequest,
                      srv: actix_web::middleware::Next<actix_web::body::BoxBody>| {
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
    let app = spawn_app(true, false, RuntimeEnvironment::NonProduction, "jp").await; // permission OK, entitlement off
    let req = actix_test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = actix_test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = actix_test::read_body_json(res).await;
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
    let app = spawn_app(false, true, RuntimeEnvironment::NonProduction, "jp").await; // entitlement OK, permission denied
    let req = actix_test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = actix_test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = actix_test::read_body_json(res).await;
    assert!(body["detail"].as_str().unwrap().contains("payments.read"));
}

#[tokio::test]
async fn the_catalog_answers_the_registered_providers_for_the_tenants_country() {
    // Non-production: the registry holds the fake provider; a JP-region
    // tenant (the fake supports US/JP) sees it available.
    let app = spawn_app(true, true, RuntimeEnvironment::NonProduction, "jp").await;
    let req = actix_test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = actix_test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = actix_test::read_body_json(res).await;
    let providers = body["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1, "one adapter registered, one entry");
    assert_eq!(providers[0]["kind"], "fake");
    assert_eq!(providers[0]["display_name"], "Test provider");
    assert_eq!(providers[0]["available"], true);
    assert_eq!(providers[0]["requires_onboarding"], true);
    assert_eq!(
        providers[0]["supported_countries"],
        serde_json::json!(["US", "JP"])
    );
}

#[tokio::test]
async fn an_unsupported_country_renders_unavailable_not_invisible() {
    // A GB-region tenant: the fake provider stays listed, `available`
    // flips false — the UI renders its unavailable state.
    let app = spawn_app(true, true, RuntimeEnvironment::NonProduction, "gb").await;
    let req = actix_test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = actix_test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = actix_test::read_body_json(res).await;
    let providers = body["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["available"], false);
}

#[tokio::test]
async fn a_production_environment_lists_no_fake_entry() {
    let app = spawn_app(true, true, RuntimeEnvironment::Production, "jp").await;
    let req = actix_test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = actix_test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = actix_test::read_body_json(res).await;
    let providers = body["providers"].as_array().unwrap();
    assert!(
        providers.is_empty(),
        "production has no Stripe adapter yet (09.2) and must list no fake entry"
    );
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

// ---------------------------------------------------------------------------
// 09.1: the registry → port → repo → outbox vertical, and the schema's
// provider CHECK admitting the fake kind.
// ---------------------------------------------------------------------------

/// The 09.1 migration (provider CHECK admits 'fake').
const FAKE_PROVIDER_MIGRATION: i64 = 20_260_824_040_000;

fn start_connection_use_case(pool: &sanvi_persistence::PgPool) -> StartProviderConnection {
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(FakeProvider::new())).unwrap();
    StartProviderConnection::new(
        TxManager::new(pool.clone()),
        Arc::new(providers),
        Arc::new(SqlxProviderConnectionRepository::new()),
        Arc::new(OutboxEventPublisher::new()),
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
    )
}

fn start_command(tenant_id: Uuid, idempotency_key: Uuid) -> StartConnectionCommand {
    StartConnectionCommand {
        tenant_id,
        provider: ProviderKind::Fake,
        country: None,
        default_currency: None,
        display_name: None,
        idempotency_key,
        actor: Uuid::now_v7(),
        ip: None,
    }
}

#[tokio::test]
async fn starting_a_fake_provider_connection_emits_the_started_event_end_to_end() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "outbox").await;

    let use_case = start_connection_use_case(&pool);
    let key = Uuid::now_v7();
    let connection = use_case
        .execute(start_command(tenant_id, key))
        .await
        .unwrap();

    assert_eq!(connection.status, ConnectionStatus::Pending);
    assert!(connection.external_account_id.starts_with("acct_fake_"));

    // The outbox row landed in the *same* transaction as the connection
    // row — the phase-01 outbox contract, proven for the fake provider.
    let events: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT event_type, payload FROM platform.outbox WHERE aggregate_id = $1")
            .bind(connection.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(events.len(), 1, "exactly one started event");
    assert_eq!(events[0].0, "payments.provider_connection_started");
    assert_eq!(events[0].1["provider"], "fake");

    // The connection row itself round-trips through the repo.
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone());
    {
        let mut scope = tx.begin_for(tenant_id).await.unwrap();
        let stored = repo
            .find_active(&mut scope, tenant_id, "fake")
            .await
            .unwrap()
            .expect("the fake connection persisted");
        assert_eq!(stored.id, connection.id);
        assert_eq!(stored.provider, ProviderKind::Fake);
        scope.commit().await.unwrap();
    }

    // Idempotent replay: the same Idempotency-Key returns the SAME
    // connection — no second row, no second event.
    let replay = use_case
        .execute(start_command(tenant_id, key))
        .await
        .unwrap();
    assert_eq!(replay.id, connection.id);
    // …and a DIFFERENT key while an active connection exists is a
    // 409-shaped conflict, not a duplicate.
    let err = use_case
        .execute(start_command(tenant_id, Uuid::now_v7()))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        sanvi_payments::PaymentsError::ConnectionExists(_)
    ));
    // …and it emitted no second event.
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE aggregate = 'payments.provider_connection'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn the_provider_check_admits_the_fake_kind_and_reverts_cleanly() {
    let (port, _node) = start_postgres().await;
    let migrator_pool = migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "fake-kind").await;

    // The CHECK admits 'fake' — the whole point of the 09.1 migration.
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone());
    let fake_connection = ProviderConnection::pending(
        Uuid::now_v7(),
        tenant_id,
        ProviderKind::Fake,
        "acct_fake_check".to_string(),
        Utc::now(),
    )
    .unwrap();
    let stored_id = fake_connection.id;
    {
        let mut scope = tx.begin_for(tenant_id).await.unwrap();
        repo.insert(&mut scope, &fake_connection).await.unwrap();
        scope.commit().await.unwrap();
    }

    // Down to before the 09.1 migration: only 'stripe_connect' is
    // admitted again, and re-applying restores the wider CHECK. The down
    // migration requires the fake row gone first (its header says so).
    // FORCE RLS applies even to the table owner, and the app role has no
    // DELETE by design — so one migrator transaction pins the tenant and
    // deletes (same connection: a pooled set_config would race).
    {
        let mut tx = migrator_pool.begin().await.unwrap();
        sqlx::query("SELECT set_config('sanvi.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM payments.provider_connections WHERE id = $1")
            .bind(stored_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let migrator = Migrator::new(migrations_dir());
    migrator
        .undo(&migrator_pool, FAKE_PROVIDER_MIGRATION - 1)
        .await
        .unwrap();
    let check: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
         WHERE conname = 'provider_connections_provider_check'
           AND conrelid = 'payments.provider_connections'::regclass",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(check.contains("'stripe_connect'"));
    assert!(!check.contains("'fake'"));

    migrator.run(&migrator_pool).await.unwrap();
    let check: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
         WHERE conname = 'provider_connections_provider_check'
           AND conrelid = 'payments.provider_connections'::regclass",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(check.contains("'stripe_connect'"));
    assert!(check.contains("'fake'"));
}

// ---------------------------------------------------------------------------
// 09.2: the idempotent create — concurrency and the orphan drill.
// ---------------------------------------------------------------------------

/// The use case plus a handle to the same fake provider (for scripting
/// and for direct adapter calls in the orphan drill).
fn session_fixture(pool: &sanvi_persistence::PgPool) -> (CreateAccountSession, FakeProvider) {
    let provider = FakeProvider::new();
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(provider.clone())).unwrap();
    let use_case = CreateAccountSession::new(
        TxManager::new(pool.clone()),
        Arc::new(providers),
        Arc::new(SqlxProviderConnectionRepository::new()),
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
    );
    (use_case, provider)
}

#[tokio::test]
async fn concurrent_creates_with_the_same_key_yield_one_account_and_one_row() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "concurrent").await;

    let use_case = Arc::new(start_connection_use_case(&pool));
    let key = Uuid::now_v7();

    // Run concurrently, not sequentially: both requests pass through the
    // provider call before either insert commits.
    let (first, second) = {
        let a = use_case.clone();
        let b = use_case.clone();
        tokio::join!(
            async move { a.execute(start_command(tenant_id, key)).await },
            async move { b.execute(start_command(tenant_id, key)).await },
        )
    };
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.id, second.id, "one connection id — the derived one");
    assert_eq!(
        first.external_account_id, second.external_account_id,
        "one provider account — idempotency-keyed"
    );

    // RLS: the count runs in a tenant-scoped tx.
    let tx = TxManager::new(pool.clone());
    let mut scope = tx.begin_for(tenant_id).await.unwrap();
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM payments.provider_connections WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(rows, 1, "exactly one connection row");
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE aggregate = 'payments.provider_connection'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "exactly one started event");
}

#[tokio::test]
async fn an_orphaned_provider_account_is_adopted_on_retry() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "orphan").await;

    // The crash window: the account was created at the provider, then the
    // process died before persistence. Simulate by calling the adapter
    // directly with the connection id the use case will derive.
    let provider = FakeProvider::new();
    let key = Uuid::now_v7();
    let orphaned = provider
        .create_account(&CreateAccountRequest {
            tenant_id,
            country: None,
            default_currency: None,
            display_name: None,
            idempotency_key: key, // the use case derives exactly this id
        })
        .await
        .unwrap();

    // The retry (same Idempotency-Key) adopts the orphaned account.
    let use_case = start_connection_use_case(&pool);
    let connection = use_case
        .execute(start_command(tenant_id, key))
        .await
        .unwrap();
    assert_eq!(connection.id, key_derived_id(tenant_id, key));
    assert_eq!(connection.external_account_id, orphaned.external_account_id);

    // One row (RLS: the count runs in a tenant-scoped tx), and the fake
    // provider holds exactly one account for the adopted key.
    let tx = TxManager::new(pool.clone());
    let mut scope = tx.begin_for(tenant_id).await.unwrap();
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM payments.provider_connections WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(rows, 1);
}

/// The derivation is the use case's private function; the test pins its
/// behaviour through the same construction (tenant ‖ key, v5).
fn key_derived_id(tenant_id: Uuid, key: Uuid) -> Uuid {
    let namespace = uuid::Uuid::from_bytes([
        0x6b, 0x9b, 0x0d, 0x1e, 0x52, 0x6f, 0x4c, 0x2e, 0xa4, 0x1d, 0x7f, 0x3c, 0x90, 0x11, 0xab,
        0x52,
    ]);
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(tenant_id.as_bytes());
    bytes[16..].copy_from_slice(key.as_bytes());
    uuid::Uuid::new_v5(&namespace, &bytes)
}

// ---------------------------------------------------------------------------
// 09.2: the account-session use case.
// ---------------------------------------------------------------------------

async fn pending_connection(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    provider: &FakeProvider,
) -> ProviderConnection {
    let account = provider
        .create_account(&CreateAccountRequest {
            tenant_id,
            country: None,
            default_currency: None,
            display_name: None,
            idempotency_key: Uuid::now_v7(),
        })
        .await
        .unwrap();
    let connection = ProviderConnection::pending(
        Uuid::now_v7(),
        tenant_id,
        ProviderKind::Fake,
        account.external_account_id.clone(),
        Utc::now(),
    )
    .unwrap();
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone());
    let mut scope = tx.begin_for(tenant_id).await.unwrap();
    repo.insert(&mut scope, &connection).await.unwrap();
    scope.commit().await.unwrap();
    connection
}

#[tokio::test]
async fn minting_a_session_moves_pending_to_onboarding_and_writes_the_audit_entry() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "session").await;
    let (use_case, provider) = session_fixture(&pool);
    let connection = pending_connection(&pool, tenant_id, &provider).await;
    let actor = Uuid::now_v7();

    let (session, transitioned) = use_case
        .execute(
            actor,
            Some("203.0.113.7".parse().unwrap()),
            tenant_id,
            &connection,
        )
        .await
        .unwrap();
    assert!(session.client_secret.starts_with("fakesession_"));
    assert!(session.expires_at > Utc::now());
    let updated = transitioned.expect("Pending moves to Onboarding on first mint");
    assert_eq!(updated.status, ConnectionStatus::Onboarding);

    // The transition persisted.
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone());
    let mut scope = tx.begin_for(tenant_id).await.unwrap();
    let stored = repo
        .find_by_id(&mut scope, tenant_id, connection.id)
        .await
        .unwrap()
        .expect("row present");

    // The audit entry: actor, tenant, IP — and never the secret. The
    // shared RLS policy admits the row's own tenant.
    let entry: (Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT actor_id, host(ip) FROM audit.entries
         WHERE action = 'payments.account_session_issued' AND resource_id = $1",
    )
    .bind(connection.id.to_string())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT after FROM audit.entries
         WHERE action = 'payments.account_session_issued' AND resource_id = $1",
    )
    .bind(connection.id.to_string())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(stored.status, ConnectionStatus::Onboarding);
    assert_eq!(entry.0, Some(actor));
    assert_eq!(entry.1.as_deref(), Some("203.0.113.7"));
    assert!(
        !after.to_string().contains(&session.client_secret),
        "the session secret never lands in the audit trail"
    );
}

#[tokio::test]
async fn a_session_is_refused_for_a_disconnected_connection() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "disconnected").await;
    let (use_case, provider) = session_fixture(&pool);
    let connection = pending_connection(&pool, tenant_id, &provider).await;

    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone());
    let mut disconnected = connection.clone();
    disconnected.disconnect(Utc::now()).unwrap();
    let mut scope = tx.begin_for(tenant_id).await.unwrap();
    repo.update(&mut scope, &disconnected).await.unwrap();
    scope.commit().await.unwrap();

    let err = use_case
        .execute(Uuid::now_v7(), None, tenant_id, &disconnected)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        sanvi_payments::PaymentsError::ConnectionNotActive(_)
    ));
}

#[tokio::test]
async fn an_expired_session_secret_is_rejected() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "expired").await;
    let (use_case, provider) = session_fixture(&pool);
    let connection = pending_connection(&pool, tenant_id, &provider).await;

    provider.script(sanvi_payments::adapters::FakeScript {
        next_session_expired: true,
        ..Default::default()
    });
    let err = use_case
        .execute(Uuid::now_v7(), None, tenant_id, &connection)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        sanvi_payments::PaymentsError::ProviderUnavailable(_)
    ));
}

#[tokio::test]
async fn creating_a_connection_writes_the_audit_entry() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "audit").await;
    let use_case = start_connection_use_case(&pool);
    let actor = Uuid::now_v7();

    let mut command = start_command(tenant_id, Uuid::now_v7());
    command.actor = actor;
    command.ip = Some("198.51.100.9".parse().unwrap());
    let connection = use_case.execute(command).await.unwrap();

    // audit.entries carries a shared RLS policy (platform scope OR the
    // row's own tenant id), so the read runs in a tenant-scoped tx.
    let tx = TxManager::new(pool.clone());
    let mut scope = tx.begin_for(tenant_id).await.unwrap();
    let entry: (Option<Uuid>, Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT actor_id, tenant_id, host(ip) FROM audit.entries
         WHERE action = 'payments.connection_created' AND resource_id = $1",
    )
    .bind(connection.id.to_string())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(entry.0, Some(actor));
    assert_eq!(entry.1, Some(tenant_id));
    assert_eq!(entry.2.as_deref(), Some("198.51.100.9"));
}

// ---------------------------------------------------------------------------
// 09.2: the new routes' gates (negative paths — the happy path runs
// through the DB-backed use-case tests above).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_connection_requires_an_idempotency_key() {
    let app = spawn_app(true, true, RuntimeEnvironment::NonProduction, "jp").await;
    let request = actix_test::TestRequest::post()
        .uri("/api/v1/tenant/payments/connections")
        .set_json(serde_json::json!({"provider": "fake"}))
        .to_request();
    let response = actix_test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn creating_a_connection_requires_payments_manage() {
    let app = spawn_app(false, true, RuntimeEnvironment::NonProduction, "jp").await;
    let request = actix_test::TestRequest::post()
        .uri("/api/v1/tenant/payments/connections")
        .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
        .set_json(serde_json::json!({"provider": "fake"}))
        .to_request();
    let response = actix_test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn creating_a_connection_requires_the_entitlement() {
    let app = spawn_app(true, false, RuntimeEnvironment::NonProduction, "jp").await;
    let request = actix_test::TestRequest::post()
        .uri("/api/v1/tenant/payments/connections")
        .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
        .set_json(serde_json::json!({"provider": "fake"}))
        .to_request();
    let response = actix_test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// 09.3: the Connect webhook transport — signature, dedupe, routing,
// out-of-order guard, DLQ and the reconciliation safety net.
// ---------------------------------------------------------------------------

use sanvi_payments::adapters::{LoggingConnectionNotifier, SqlxWebhookEventRepository};
use sanvi_payments::application::{SyncConnections, WebhookDispatcher, WebhookReceiver};
use sanvi_payments::domain::{
    ConnectionLifecycleStore, ConnectionNotifier, EventPublisher, WebhookEventRepository,
};

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// Sign a payload the way Stripe does (`t=…,v1=…` over `{t}.{body}`).
fn sign_webhook(secret: &str, payload: &[u8]) -> String {
    use hmac::Mac;
    let timestamp = Utc::now().timestamp();
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.{}", String::from_utf8_lossy(payload)).as_bytes());
    format!(
        "t={timestamp},v1={}",
        hex::encode(mac.finalize().into_bytes())
    )
}

/// A signed `account.updated` envelope for the given account.
fn account_event(event_id: &str, account: &str, created: i64) -> serde_json::Value {
    serde_json::json!({
        "id": event_id,
        "type": "account.updated",
        "created": created,
        "account": account,
        "data": {"object": {"id": account}}
    })
}

fn epoch(secs_from_now: i64) -> i64 {
    Utc::now().timestamp() + secs_from_now
}

/// The full 09.3 stack on a real pool: receiver (fake-secret verifier,
/// loopback allow-list), dispatcher and reconciler sharing one fake
/// provider handle the test scripts.
struct WebhookFixture {
    receiver: Arc<WebhookReceiver>,
    dispatcher: Arc<WebhookDispatcher>,
    sync: Arc<SyncConnections>,
    webhooks: Arc<dyn WebhookEventRepository>,
    tx: TxManager,
    provider: FakeProvider,
}

async fn webhook_fixture(pool: &sanvi_persistence::PgPool) -> WebhookFixture {
    let provider = FakeProvider::new();
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(provider.clone())).unwrap();
    let registry = Arc::new(providers);
    let webhooks: Arc<dyn WebhookEventRepository> = Arc::new(SqlxWebhookEventRepository::new());
    let lifecycle: Arc<dyn ConnectionLifecycleStore> =
        Arc::new(SqlxProviderConnectionRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    let notifier: Arc<dyn ConnectionNotifier> = Arc::new(LoggingConnectionNotifier::new());
    let tx = TxManager::new(pool.clone());
    let receiver = Arc::new(WebhookReceiver::new(
        tx.clone(),
        webhooks.clone(),
        // The same adapter family the composition root wires: here the
        // fake's secret stands in for Stripe's.
        Arc::new(provider.clone()),
        vec!["127.0.0.1".to_string(), "::1".to_string()],
    ));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        tx.clone(),
        webhooks.clone(),
        lifecycle.clone(),
        registry.clone(),
        events.clone(),
        notifier.clone(),
    ));
    let sync = Arc::new(SyncConnections::new(
        tx.clone(),
        lifecycle.clone(),
        registry,
        events,
        notifier,
        chrono::Duration::zero(),
    ));
    WebhookFixture {
        receiver,
        dispatcher,
        sync,
        webhooks,
        tx,
        provider,
    }
}

impl WebhookFixture {
    /// Deliver a signed event through the HTTP-boundary contract.
    async fn deliver(
        &self,
        event: &serde_json::Value,
    ) -> Result<bool, sanvi_payments::domain::PaymentsError> {
        let body = serde_json::to_vec(event).unwrap();
        let signature = sign_webhook(sanvi_payments::adapters::FAKE_WEBHOOK_SECRET, &body);
        self.receiver
            .receive(&body, Some(&signature), Some("127.0.0.1".parse().unwrap()))
            .await
    }

    async fn connection_count_for(&self, account: &str) -> i64 {
        let mut scope = self
            .tx
            .begin_unscoped("test_count_connections", "tests")
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM payments.provider_connections WHERE external_account_id = $1",
        )
        .bind(account)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
        scope.rollback().await.unwrap();
        count
    }

    async fn outbox_events(&self, event_type: &str) -> i64 {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM platform.outbox WHERE event_type = $1")
                .bind(event_type)
                .fetch_one(self.tx.pool())
                .await
                .unwrap();
        count
    }
}

#[tokio::test]
async fn the_connect_endpoint_verifies_the_signature_before_storing_and_dedupes_redeliveries() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let fixture = webhook_fixture(&pool).await;

    let event = account_event("evt_sig_1", "acct_sig", epoch(0));
    let body = serde_json::to_vec(&event).unwrap();

    // No signature header: rejected before anything is parsed or stored.
    let err = fixture
        .receiver
        .receive(&body, None, Some("127.0.0.1".parse().unwrap()))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        sanvi_payments::domain::PaymentsError::WebhookRejected(_)
    ));
    assert_eq!(fixture.connection_count_for("acct_sig").await, 0);

    // Signed with the phase-04 PLATFORM secret: must not verify against
    // the Connect endpoint — the cross-secret invariant at the transport.
    let platform_signed = sign_webhook("whsec_platform_test", &body);
    assert!(
        fixture
            .receiver
            .receive(
                &body,
                Some(&platform_signed),
                Some("127.0.0.1".parse().unwrap())
            )
            .await
            .is_err()
    );

    // Tampered body under a valid signature for the original: rejected.
    let valid_for_original = sign_webhook(sanvi_payments::adapters::FAKE_WEBHOOK_SECRET, &body);
    let tampered =
        br#"{"id":"evt_sig_1","type":"account.updated","created":1,"account":"acct_evil"}"#;
    assert!(
        fixture
            .receiver
            .receive(
                tampered,
                Some(&valid_for_original),
                Some("127.0.0.1".parse().unwrap())
            )
            .await
            .is_err()
    );

    // Correctly signed: stored once; a redelivery is a no-op, both acked.
    assert!(
        fixture.deliver(&event).await.unwrap(),
        "first delivery stores"
    );
    assert!(
        !fixture.deliver(&event).await.unwrap(),
        "redelivery dedupes"
    );
    let mut scope = fixture
        .tx
        .begin_unscoped("test_count_events", "tests")
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM payments.webhook_events")
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(rows, 1, "one row per event id");
}

#[tokio::test]
async fn an_event_for_an_unknown_account_is_recorded_but_creates_no_rows() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "unknown-acct").await;
    let _ = tenant_id;
    let fixture = webhook_fixture(&pool).await;

    let event = account_event("evt_orphan_1", "acct_never_seen", epoch(0));
    assert!(fixture.deliver(&event).await.unwrap());
    let report = fixture.dispatcher.dispatch(10).await.unwrap();

    // Acknowledged as skipped (never retried into the DLQ), recorded with
    // its account id, alerted via the error log + metric — and no
    // connection row exists for the unknown account.
    assert_eq!(report.skipped, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(fixture.connection_count_for("acct_never_seen").await, 0);
    let mut scope = fixture
        .tx
        .begin_unscoped("test_unknown_event", "tests")
        .await
        .unwrap();
    let row: (Option<String>, Option<DateTime<Utc>>, i32) = sqlx::query_as(
        "SELECT account_id, processed_at, attempts FROM payments.webhook_events
         WHERE stripe_event_id = 'evt_orphan_1'",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(
        row.0.as_deref(),
        Some("acct_never_seen"),
        "the event is recorded"
    );
    assert!(row.1.is_some(), "acknowledged, not retried");
    assert_eq!(row.2, 0);
}

#[tokio::test]
async fn the_same_event_delivered_twice_produces_exactly_one_state_change() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "dedupe").await;
    let fixture = webhook_fixture(&pool).await;
    let connection = pending_connection(&pool, tenant_id, &fixture.provider).await;
    fixture
        .provider
        .approve_onboarding(&connection.external_account_id);

    let event = account_event(
        "evt_once_1",
        &connection.external_account_id,
        epoch(5), // after the row's last_synced_at → fresh
    );
    assert!(fixture.deliver(&event).await.unwrap());
    let first = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(first.processed, 1);

    // The redelivery inserts nothing new; there is nothing left to process.
    assert!(!fixture.deliver(&event).await.unwrap());
    let second = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(second.processed, 0);

    // Exactly one activation in the outbox despite two deliveries.
    assert_eq!(
        fixture
            .outbox_events("payments.provider_connection_activated")
            .await,
        1
    );

    let repo = SqlxProviderConnectionRepository::new();
    let mut scope = fixture.tx.begin_for(tenant_id).await.unwrap();
    let stored = repo
        .find_by_id(&mut scope, tenant_id, connection.id)
        .await
        .unwrap()
        .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(stored.status, ConnectionStatus::Active);
    assert!(stored.connected_at.is_some());
}

#[tokio::test]
async fn a_stale_pending_event_leaves_an_active_connection_active() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "stale").await;
    let fixture = webhook_fixture(&pool).await;
    let connection = pending_connection(&pool, tenant_id, &fixture.provider).await;
    fixture
        .provider
        .approve_onboarding(&connection.external_account_id);

    // Activation lands first (created now+60).
    let fresh = account_event(
        "evt_stale_fresh",
        &connection.external_account_id,
        epoch(60),
    );
    assert!(fixture.deliver(&fresh).await.unwrap());
    assert_eq!(fixture.dispatcher.dispatch(10).await.unwrap().processed, 1);

    // …then an OLDER event (created now+10) arrives late. It describes a
    // state strictly older than what we already applied: dropped, not
    // re-applied.
    let stale = account_event("evt_stale_late", &connection.external_account_id, epoch(10));
    assert!(fixture.deliver(&stale).await.unwrap());
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.skipped, 1, "the stale event is skipped");
    assert_eq!(report.processed, 0);

    let repo = SqlxProviderConnectionRepository::new();
    let mut scope = fixture.tx.begin_for(tenant_id).await.unwrap();
    let stored = repo
        .find_by_id(&mut scope, tenant_id, connection.id)
        .await
        .unwrap()
        .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(stored.status, ConnectionStatus::Active, "still active");
}

#[tokio::test]
async fn a_poison_event_is_dead_lettered_does_not_block_the_queue_and_is_replayable() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "poison").await;
    let fixture = webhook_fixture(&pool).await;

    // A connection whose provider adapter is NOT in this registry: every
    // processing attempt fails — the poison.
    let poison_connection = ProviderConnection::pending(
        Uuid::now_v7(),
        tenant_id,
        ProviderKind::StripeConnect,
        "acct_poison".to_string(),
        Utc::now(),
    )
    .unwrap();
    {
        let repo = SqlxProviderConnectionRepository::new();
        let mut scope = fixture.tx.begin_for(tenant_id).await.unwrap();
        repo.insert(&mut scope, &poison_connection).await.unwrap();
        scope.commit().await.unwrap();
    }
    let poison = account_event("evt_poison_1", "acct_poison", epoch(5));
    assert!(fixture.deliver(&poison).await.unwrap());

    // Five attempts exhaust the budget…
    for _ in 0..5 {
        let report = fixture.dispatcher.dispatch(10).await.unwrap();
        assert_eq!(report.failed, 1);
    }
    assert_eq!(
        fixture
            .webhooks
            .dead_letter_count(
                &mut fixture
                    .tx
                    .begin_unscoped("test_dlq", "tests")
                    .await
                    .unwrap()
            )
            .await
            .unwrap(),
        1
    );

    // …and the dead-lettered event stops being fetched: a healthy event
    // behind it processes anyway.
    let healthy_tenant = seed_tenant(&pool, "healthy").await;
    let healthy = pending_connection(&pool, healthy_tenant, &fixture.provider).await;
    fixture
        .provider
        .approve_onboarding(&healthy.external_account_id);
    let good = account_event("evt_healthy_1", &healthy.external_account_id, epoch(5));
    assert!(fixture.deliver(&good).await.unwrap());
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1, "the poison does not block the queue");
    assert_eq!(report.failed, 0, "dead-lettered events are not fetched");

    // Replay from the operator surface (`apps/cli` calls exactly this):
    // the budget resets and the event becomes eligible again.
    let mut scope = fixture
        .tx
        .begin_unscoped("test_replay", "tests")
        .await
        .unwrap();
    let replayed = fixture
        .webhooks
        .replay(&mut scope, "evt_poison_1")
        .await
        .unwrap();
    scope.commit().await.unwrap();
    assert!(replayed);
    let mut scope = fixture
        .tx
        .begin_unscoped("test_after_replay", "tests")
        .await
        .unwrap();
    let pending_again = fixture
        .webhooks
        .fetch_unprocessed(&mut scope, 10)
        .await
        .unwrap();
    scope.rollback().await.unwrap();
    assert!(
        pending_again
            .iter()
            .any(|record| record.stripe_event_id == "evt_poison_1"),
        "the replayed event is back in the queue"
    );
}

#[tokio::test]
async fn the_lost_webhook_drill_reconciliation_alone_reaches_active() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "drill").await;
    let fixture = webhook_fixture(&pool).await;
    let connection = pending_connection(&pool, tenant_id, &fixture.provider).await;

    // The tenant finishes onboarding at the provider — but every webhook
    // is lost. No events are delivered at all.
    fixture
        .provider
        .approve_onboarding(&connection.external_account_id);

    let report = fixture.sync.execute(10).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.updated, 1, "the reconciliation repairs the status");

    let repo = SqlxProviderConnectionRepository::new();
    let mut scope = fixture.tx.begin_for(tenant_id).await.unwrap();
    let stored = repo
        .find_by_id(&mut scope, tenant_id, connection.id)
        .await
        .unwrap()
        .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(stored.status, ConnectionStatus::Active);
    assert_eq!(
        fixture
            .outbox_events("payments.provider_connection_activated")
            .await,
        1,
        "the drill emits the same activation event a webhook would"
    );

    // A second pass with nothing changed updates nothing.
    let again = fixture.sync.execute(10).await.unwrap();
    assert_eq!(again.updated, 0);
}

// ---------------------------------------------------------------------------
// 09.3: the enriched connection view (FR-903) — server-computed readiness,
// requirement render keys and blockers.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_connection_view_maps_requirements_blockers_and_readiness() {
    use sanvi_payments::domain::{CapabilitySet, CapabilityStatus, RequirementSummary};
    let config = PaymentsConfig::default();
    let mut connection = ProviderConnection::pending(
        Uuid::now_v7(),
        Uuid::now_v7(),
        ProviderKind::StripeConnect,
        "acct_view".to_string(),
        Utc::now(),
    )
    .unwrap();

    // Onboarding with a pending capability and mixed requirements.
    let snapshot = sanvi_payments::domain::ports::ProviderAccountStatus {
        external_account_id: connection.external_account_id.clone(),
        capabilities: CapabilitySet::empty().with("card_payments", CapabilityStatus::Pending),
        requirements: RequirementSummary {
            currently_due: vec![
                "business_profile.url".to_string(),
                "brand.new.requirement".to_string(),
            ],
            eventually_due: vec!["individual.verification.document".to_string()],
            past_due: vec!["individual.id_number".to_string()],
            deadline: chrono::DateTime::from_timestamp(epoch(86_400), 0),
        },
        country: None,
        default_currency: None,
        status_hint: None,
    };
    connection
        .apply_status(&snapshot, ConnectionStatus::Onboarding, Utc::now())
        .unwrap();

    let view = sanvi_payments::adapters::http::handlers::ConnectionView::of(&connection, &config);
    assert!(
        !view.can_accept_payments,
        "pending capability cannot accept"
    );
    assert_eq!(view.status, "onboarding");
    // Capabilities render flat, normalised from the v2-nested form.
    assert_eq!(view.capabilities["card_payments"], "pending");
    // Known codes map to their message keys; unknown ones fall back.
    assert_eq!(
        view.requirements.currently_due[0].summary_key,
        "payments.req.business_url"
    );
    assert_eq!(
        view.requirements.currently_due[1].summary_key, "payments.req.unmapped",
        "a requirement Stripe adds before the map learns it degrades, never breaks"
    );
    assert_eq!(
        view.requirements.eventually_due[0].summary_key,
        "payments.req.id_document"
    );
    assert!(view.requirements.deadline.is_some());
    // Blockers: capability inactive while onboarding + past-due items.
    let blocker_keys: Vec<&str> = view
        .blockers
        .iter()
        .map(|blocker| blocker.summary_key.as_str())
        .collect();
    assert!(blocker_keys.contains(&"payments.blocker.card_payments_inactive"));
    assert!(blocker_keys.contains(&"payments.blocker.requirements_past_due"));

    // Once active with nothing past due: readiness flips, blockers clear.
    let active_snapshot = sanvi_payments::domain::ports::ProviderAccountStatus {
        capabilities: CapabilitySet::empty().with("card_payments", CapabilityStatus::Active),
        requirements: RequirementSummary::default(),
        ..snapshot
    };
    connection
        .apply_status(&active_snapshot, ConnectionStatus::Active, Utc::now())
        .unwrap();
    let view = sanvi_payments::adapters::http::handlers::ConnectionView::of(&connection, &config);
    assert!(view.can_accept_payments);
    assert!(
        view.blockers.is_empty(),
        "no blockers once active and clean"
    );
}

#[tokio::test]
async fn get_connection_returns_the_enriched_truthful_view() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "get-view").await;
    let fixture = webhook_fixture(&pool).await;
    let connection = pending_connection(&pool, tenant_id, &fixture.provider).await;

    // Sync once so the row carries the fake's seeded requirements (the
    // create use case does this initial projection; the raw helper does
    // not).
    let snapshot = fixture
        .provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    let mut synced = connection.clone();
    synced.refresh_snapshot(&snapshot, Utc::now());
    {
        let repo = SqlxProviderConnectionRepository::new();
        let mut scope = fixture.tx.begin_for(tenant_id).await.unwrap();
        repo.update(&mut scope, &synced).await.unwrap();
        scope.commit().await.unwrap();
    }

    // The route with REAL middleware products and a REAL pool.
    let subject = Subject {
        user_id: Uuid::now_v7(),
        session_id: "sess_test".to_string(),
        aal: "aal2".to_string(),
        methods: vec!["password".to_string()],
        tenant_id: Some(tenant_id),
        role_ids: vec![],
        authenticated_at: Some(Utc::now()),
    };
    let tenant_context = TenantContext {
        tenant_id,
        slug: "get-view".to_string(),
        display_name: "Get View".to_string(),
        status: sanvi_http::tenancy::TenantRuntimeStatus::Active,
        region: "jp".to_string(),
        default_locale: "en".to_string(),
        resolution_source: sanvi_http::tenancy::ResolutionSource::Subdomain,
    };
    let permission_gate: Arc<dyn PermissionGate> = Arc::new(FakePermissionGate { allowed: true });
    let feature_gate: Arc<dyn FeatureGate> = Arc::new(FakeFeatureGate { enabled: true });
    let app = actix_test::init_service(
        App::new()
            .app_data(web::Data::new({
                let mut state = {
                    // Rebuild the state on the real pool (the lazy-pool
                    // helper would never reach the database).
                    let prometheus_registry = prometheus::Registry::new();
                    let providers = ProviderRegistry::new();
                    PaymentsState {
                        config: PaymentsConfig::default(),
                        metrics: Arc::new(PaymentsMetrics::new(&prometheus_registry)),
                        list_providers: Arc::new(ListProviders::new(Arc::new(
                            RegistryProviderCatalog::new(
                                Arc::new(providers),
                                RuntimeEnvironment::Production,
                            ),
                        ))),
                        registry: Arc::new(ProviderRegistry::new()),
                        tx: TxManager::new(pool.clone()),
                        connections: Arc::new(SqlxProviderConnectionRepository::new()),
                        create_connection: Arc::new(StartProviderConnection::new(
                            TxManager::new(pool.clone()),
                            Arc::new(ProviderRegistry::new()),
                            Arc::new(SqlxProviderConnectionRepository::new()),
                            Arc::new(OutboxEventPublisher::new()),
                            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
                        )),
                        account_session: Arc::new(CreateAccountSession::new(
                            TxManager::new(pool.clone()),
                            Arc::new(ProviderRegistry::new()),
                            Arc::new(SqlxProviderConnectionRepository::new()),
                            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
                        )),
                        webhook_receiver: Arc::new(WebhookReceiver::new(
                            TxManager::new(pool.clone()),
                            Arc::new(SqlxWebhookEventRepository::new()),
                            Arc::new(FakeProvider::new()),
                            vec!["127.0.0.1".to_string()],
                        )),
                    }
                };
                state.config.requirement_keys.insert(
                    "business_profile.url".to_string(),
                    "payments.req.business_url".to_string(),
                );
                state
            }))
            .app_data(web::Data::new(permission_gate))
            .app_data(web::Data::new(feature_gate))
            .wrap(actix_web::middleware::from_fn(
                move |req: actix_web::dev::ServiceRequest,
                      srv: actix_web::middleware::Next<actix_web::body::BoxBody>| {
                    let subject = subject.clone();
                    let tenant_context = tenant_context.clone();
                    async move {
                        req.extensions_mut().insert(subject);
                        req.extensions_mut().insert(tenant_context);
                        srv.call(req).await
                    }
                },
            ))
            .service(web::scope("/api/v1/tenant").configure(payments_tenant_scope_routes)),
    )
    .await;

    let request = actix_test::TestRequest::get()
        .uri(&format!(
            "/api/v1/tenant/payments/connections/{}",
            connection.id
        ))
        .to_request();
    let response = actix_test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = actix_test::read_body_json(response).await;
    assert_eq!(body["status"], "pending");
    assert_eq!(body["can_accept_payments"], false);
    assert_eq!(
        body["blockers"][0]["summary_key"],
        "payments.blocker.onboarding_not_started"
    );
    // The fake seeds requirements at create time; their codes map to keys.
    let currently_due = body["requirements"]["currently_due"].as_array().unwrap();
    assert!(!currently_due.is_empty());
    for entry in currently_due {
        let key = entry["summary_key"].as_str().unwrap();
        assert!(
            key.starts_with("payments.req."),
            "render key present: {key}"
        );
    }
}
