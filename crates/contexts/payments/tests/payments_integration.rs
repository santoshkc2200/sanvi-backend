//! The payments context's integration tests (sub-phases 09.0–09.5).
//!
//! Real Postgres, the `sanvi_app` role (NOBYPASSRLS), full
//! migrations — the same recipe the domains/billing suites use. 09.0
//! covered the schema, RLS and the catalog route's gates; 09.1 added the
//! registry-derived catalog and the registry → port → repo → outbox
//! vertical; 09.2 the idempotent connection create, session refusals and
//! audit trail; 09.4 checkout on the connected account with webhook-driven
//! truth; 09.5 the refund use case's idempotency and recovery, dispute
//! reconciliation, deep links, and the streamed tenant-scoped CSV export.

use std::sync::Arc;

// `test` is aliased: `actix_web` re-exports both a `test` module (these
// helpers) and a `#[test]`-shadowing attribute macro at the same path, so a
// bare `use actix_web::test;` would silently turn the plain `#[test]`
// below into `#[actix_web::test]` and demand an async fn.
use actix_web::{App, HttpMessage, http::StatusCode, test as actix_test, web};
use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use uuid::Uuid;

use sanvi_http::{FeatureGate, PermissionGate, ResourceRef, Subject, tenancy::TenantContext};
use sanvi_payments::adapters::http::PaymentsState;
use sanvi_payments::adapters::repos::{
    SqlxCheckoutRepository, SqlxDisputeRepository, SqlxPaymentRepository,
    SqlxProviderConnectionRepository, SqlxRefundRepository,
};
use sanvi_payments::adapters::{
    FakeProvider, FakeScript, FixedWindowThrottle, OutboxEventPublisher, PaymentsMetrics,
    SystemIdGenerator,
};
use sanvi_payments::application::{
    ApplicationFeePolicy, CreateAccountSession, CreateCheckout, ListProviders, NewCheckoutCommand,
    ProviderRegistry, RegistryProviderCatalog, RuntimeEnvironment, StartConnectionCommand,
    StartProviderConnection,
};
use sanvi_payments::config::PaymentsConfig;
use sanvi_payments::domain::ports::{CreateAccountRequest, PaymentProviderPort};
use sanvi_payments::domain::provider::{ConnectionStatus, ProviderKind};
use sanvi_payments::domain::{
    CheckoutId, ConnectionId, DisputeId, Money, PaymentId, PayoutId, ProviderConnection,
    ProviderConnectionRepository,
};
use sanvi_payments::routes::payments_tenant_scope_routes;
use sanvi_persistence::{Migrator, TxManager};
use sanvi_privacy::domain::{DataClass, EraseMode, PersonalDataProvider, SubjectRef};

/// Test double for the phase-08 verified-domain allow-list: exactly one
/// hostname is live, for whichever tenant asks — cross-tenant behaviour
/// of the *projection* belongs to phase 08's own suite.
struct AllowShopTest;

#[async_trait::async_trait]
impl sanvi_payments::domain::ports::TenantDomainAllowList for AllowShopTest {
    async fn allows(
        &self,
        _tenant_id: TenantId,
        hostname: &str,
    ) -> Result<bool, sanvi_payments::PaymentsError> {
        Ok(hostname == "shop.test")
    }
}

/// Fees are off everywhere in this release; the test twin proves the
/// wiring point exists by turning on only where a test opts in.
struct TestFeePolicy(bool);

/// The test twin of the platform-fee store: no tenant is ever configured
/// (the release default). Fee-on paths drive [`TestFeePolicy`] instead.
#[derive(Default)]
struct NoFeeStore;

#[async_trait::async_trait]
impl sanvi_payments::domain::ports::ApplicationFeeStore for NoFeeStore {
    async fn configuration_for(
        &self,
        _tenant_id: TenantId,
    ) -> Result<Option<sanvi_payments::domain::fees::FeeConfiguration>, sanvi_payments::PaymentsError>
    {
        Ok(None)
    }
}

#[async_trait::async_trait]
impl ApplicationFeePolicy for TestFeePolicy {
    async fn application_fee(
        &self,
        _tenant_id: TenantId,
        amount: &sanvi_payments::Money,
    ) -> Option<sanvi_payments::Money> {
        if self.0 {
            Some(
                sanvi_payments::Money::new(amount.amount_minor / 10, amount.currency.as_str())
                    .ok()?,
            )
        } else {
            None
        }
    }
}

fn migrations_dir() -> std::path::PathBuf {
    sanvi_testsupport::migrations_dir().into()
}

/// The migration this slice ships (up/down round-trip target).
const PAYMENTS_MIGRATION: i64 = 20_260_823_000_000;

async fn seed_tenant(pool: &sqlx::PgPool, slug: &str) -> TenantId {
    let tenant_id =
        TenantId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id"));
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, $2, $3, 'active', 'jp', 'en')",
    )
    .bind(tenant_id.as_i64())
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
    let db = sanvi_testsupport::test_db().await;
    let migrator_pool = db.migrator_pool().await;
    let pool = db.app_pool();

    let tenant_id = seed_tenant(&pool, "round-trip").await;
    let connection = ProviderConnection::pending(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        ProviderKind::StripeConnect,
        format!("acct_{}", sanvi_testsupport::test_ids().next_id().unwrap()),
        Utc::now(),
    )
    .unwrap();

    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    {
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
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

#[tokio::test]
async fn retention_override_rollback_preserves_an_operator_owned_rule() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.migrator_pool().await;

    // Simulate an operator's pre-existing rule winning the forward
    // migration's ON CONFLICT. The reverse must not delete that rule.
    sqlx::query(
        "UPDATE privacy.retention_rules
         SET updated_by = NULL
         WHERE data_class = 'transaction_record'",
    )
    .execute(&pool)
    .await
    .unwrap();

    Migrator::new(migrations_dir())
        .undo(&pool, 20_260_824_000_000)
        .await
        .unwrap();
    let retained: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM privacy.retention_rules
             WHERE data_class = 'transaction_record'
         )",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(retained, "rollback must preserve an operator-owned rule");
}

// ---------------------------------------------------------------------------
// RLS: tenant isolation on the tenant-scoped tables.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tenant_a_cannot_read_tenant_bs_connections() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();

    let tenant_a = seed_tenant(&pool, "alpha").await;
    let tenant_b = seed_tenant(&pool, "beta").await;

    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let connection = ProviderConnection::pending(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_a,
        ProviderKind::StripeConnect,
        "acct_alpha".to_string(),
        Utc::now(),
    )
    .unwrap();
    {
        let mut scope = tx.begin_for(tenant_a.as_i64()).await.unwrap();
        repo.insert(&mut scope, &connection).await.unwrap();
        scope.commit().await.unwrap();
    }

    // Through the repo: A sees its connection, B sees nothing.
    {
        let mut scope = tx.begin_for(tenant_a.as_i64()).await.unwrap();
        assert_eq!(repo.list(&mut scope, tenant_a).await.unwrap().len(), 1);
        scope.commit().await.unwrap();
    }
    {
        let mut scope = tx.begin_for(tenant_b.as_i64()).await.unwrap();
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
        let mut scope = tx.begin_for(tenant_a.as_i64()).await.unwrap();
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
        let mut scope = tx.begin_for(tenant_b.as_i64()).await.unwrap();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "single").await;

    // Two live rows for the same provider collide (partial unique index)…
    let first = ProviderConnection::pending(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        ProviderKind::StripeConnect,
        "acct_one".to_string(),
        Utc::now(),
    )
    .unwrap();
    let second = ProviderConnection::pending(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        ProviderKind::StripeConnect,
        "acct_two".to_string(),
        Utc::now(),
    )
    .unwrap();
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    {
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
        repo.insert(&mut scope, &first).await.unwrap();
        scope.commit().await.unwrap();
    }
    {
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
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
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
        sqlx::query(
            "UPDATE payments.provider_connections
             SET disconnected_at = now(), status = 'disconnected' WHERE id = $1",
        )
        .bind(first.id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }
    {
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
        repo.insert(&mut scope, &second).await.unwrap();
        scope.commit().await.unwrap();
    }
    {
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
        let history = repo.list(&mut scope, tenant_id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].status, ConnectionStatus::Pending);
        assert_eq!(history[1].status, ConnectionStatus::Disconnected);
        scope.commit().await.unwrap();
    }
}

#[tokio::test]
async fn webhook_events_are_platform_scoped_not_tenant_readable() {
    let db = sanvi_testsupport::test_db().await;

    // The four tenant-scoped tables carry the tenant_isolation policy;
    // webhook_events deliberately does not (Connect events arrive before
    // the tenant is known and map to one only through
    // external_account_id — the table is touched solely through
    // platform-scope transactions).
    let admin = db.migrator_pool().await;

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

    // TASK-028: the no-RLS set is the webhook history (now a partitioned
    // parent plus its monthly children, all platform-scoped) and the
    // webhook_event_dedup claim sidecar. Everything else in the schema
    // stays tenant-isolated.
    let no_rls: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'payments' AND rowsecurity = false",
    )
    .fetch_all(&admin)
    .await
    .unwrap();
    let mut expected: Vec<String> = no_rls
        .iter()
        .map(|(t,)| t.clone())
        .filter(|t| {
            t == "webhook_events"
                || t == "webhook_event_dedup"
                || (t.starts_with("webhook_events_")
                    && t["webhook_events_".len()..]
                        .chars()
                        .all(|c| c.is_ascii_digit()))
        })
        .collect();
    expected.sort();
    let mut actual: Vec<String> = no_rls.iter().map(|(t,)| t.clone()).collect();
    actual.sort();
    assert_eq!(
        actual, expected,
        "payments' no-RLS set is exactly the webhook history parent, its monthly \
         children and the dedup sidecar — platform-scoped, joined to tenants \
         only through external_account_id"
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
        _write: bool,
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
        _tenant_id: TenantId,
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
    let tx = TxManager::new(lazy_pool, sanvi_testsupport::test_ids());
    let connections: Arc<dyn sanvi_payments::domain::ProviderConnectionRepository> =
        Arc::new(SqlxProviderConnectionRepository::new());
    let checkouts: Arc<dyn sanvi_payments::domain::CheckoutRepository> =
        Arc::new(SqlxCheckoutRepository::new());
    let payments: Arc<dyn sanvi_payments::domain::PaymentRepository> =
        Arc::new(SqlxPaymentRepository::new());
    let refunds: Arc<dyn sanvi_payments::domain::RefundRepository> =
        Arc::new(SqlxRefundRepository::new());
    let disputes: Arc<dyn sanvi_payments::domain::DisputeRepository> =
        Arc::new(SqlxDisputeRepository::new());
    let audit: Arc<dyn sanvi_audit::domain::AuditWriter> = Arc::new(
        sanvi_audit::adapters::SqlxAuditWriter::new(sanvi_testsupport::test_ids()),
    );

    PaymentsState {
        config: PaymentsConfig::default(),
        metrics: Arc::new(PaymentsMetrics::new(&prometheus_registry)),
        list_providers: Arc::new(ListProviders::new(catalog)),
        registry: providers.clone(),
        tx: tx.clone(),
        connections: connections.clone(),
        create_connection: Arc::new(StartProviderConnection::new(
            tx.clone(),
            providers.clone(),
            connections.clone(),
            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
            audit.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        account_session: Arc::new(sanvi_payments::application::CreateAccountSession::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            providers.clone(),
            connections,
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
                sanvi_testsupport::test_ids(),
            )),
        )),
        disconnect_connection: Arc::new(sanvi_payments::application::DisconnectConnection::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            Arc::new(SqlxProviderConnectionRepository::new()),
            payments.clone(),
            disputes.clone(),
            Arc::new(sanvi_payments::adapters::SqlxPayoutRepository::new()),
            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
            audit.clone(),
        )),
        webhook_receiver: Arc::new(sanvi_payments::application::WebhookReceiver::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            Arc::new(sanvi_payments::adapters::SqlxWebhookEventRepository::new()),
            Arc::new(FakeProvider::new()),
            vec!["127.0.0.1".to_string(), "::1".to_string()],
        )),
        checkouts: checkouts.clone(),
        create_checkout: Arc::new(CreateCheckout::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            providers.clone(),
            Arc::new(SqlxProviderConnectionRepository::new()),
            Arc::new(SqlxCheckoutRepository::new()),
            Arc::new(AllowShopTest),
            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
            Arc::new(TestFeePolicy(false)),
            Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new()),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        checkout_throttle: Arc::new(FixedWindowThrottle::new(1000)),
        payments: payments.clone(),
        refunds: refunds.clone(),
        disputes: disputes.clone(),
        list_payments: Arc::new(sanvi_payments::application::ListPayments::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            payments.clone(),
        )),
        get_payment: Arc::new(sanvi_payments::application::GetPaymentDetail::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            payments.clone(),
            refunds.clone(),
            disputes.clone(),
            checkouts.clone(),
            Arc::new(SqlxProviderConnectionRepository::new()),
        )),
        issue_refund: Arc::new(sanvi_payments::application::IssueRefund::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            providers.clone(),
            payments.clone(),
            refunds.clone(),
            Arc::new(SqlxProviderConnectionRepository::new()),
            checkouts.clone(),
            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
                sanvi_testsupport::test_ids(),
            )),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        list_disputes: Arc::new(sanvi_payments::application::ListDisputes::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            disputes.clone(),
            payments.clone(),
            checkouts.clone(),
            Arc::new(SqlxProviderConnectionRepository::new()),
        )),
        export_payments: Arc::new(sanvi_payments::application::ExportPayments::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            payments.clone(),
            refunds.clone(),
            disputes.clone(),
            checkouts.clone(),
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
                sanvi_testsupport::test_ids(),
            )),
        )),
        payouts: Arc::new(sanvi_payments::adapters::SqlxPayoutRepository::new()),
        list_payouts: Arc::new(sanvi_payments::application::ListPayouts::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            Arc::new(SqlxProviderConnectionRepository::new()),
            providers.clone(),
        )),
        tax_settings: {
            let repo: Arc<dyn sanvi_payments::domain::TaxSettingsRepository> =
                Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new());
            repo
        },
        manage_tax_settings: Arc::new(sanvi_payments::application::ManageTaxSettings::new(
            TxManager::new(
                sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                    .expect("lazy pool construction is configuration only"),
                sanvi_testsupport::test_ids(),
            ),
            Arc::new(SqlxProviderConnectionRepository::new()),
            providers.clone(),
            Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new()),
        )),
        fee_store: Arc::new(NoFeeStore),
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
    let tenant_id =
        TenantId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id"));
    let subject = Subject {
        user_id: UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
async fn the_catalog_route_is_readable_without_the_entitlement() {
    let app = spawn_app(true, false, RuntimeEnvironment::NonProduction, "jp").await; // permission OK, entitlement off
    let req = actix_test::TestRequest::get()
        .uri("/api/v1/tenant/payments/providers")
        .to_request();
    let res = actix_test::call_service(&app, req).await;
    // The catalog is the menu, not the meal: an un-entitled tenant sees which
    // providers exist so the admin can show the cards with the connect CTA
    // replaced by an upgrade prompt. Connecting stays entitlement-gated.
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = actix_test::read_body_json(res).await;
    assert!(!body["providers"].as_array().unwrap().is_empty());
    // The other half of this boundary — connecting without the entitlement is
    // still 403 — is `creating_a_connection_requires_the_entitlement`.
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
        TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
        Arc::new(providers),
        Arc::new(SqlxProviderConnectionRepository::new()),
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
            sanvi_testsupport::test_ids(),
        )),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    )
}

fn start_command(tenant_id: TenantId, idempotency_key: Uuid) -> StartConnectionCommand {
    StartConnectionCommand {
        tenant_id,
        provider: ProviderKind::Fake,
        country: None,
        default_currency: None,
        display_name: None,
        idempotency_key,
        actor: UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        ip: None,
    }
}

#[tokio::test]
async fn starting_a_fake_provider_connection_emits_the_started_event_end_to_end() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
            .bind(connection.id.as_i64())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(events.len(), 1, "exactly one started event");
    assert_eq!(events[0].0, "payments.provider_connection_started");
    assert_eq!(events[0].1["provider"], "fake");

    // The connection row itself round-trips through the repo.
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    {
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
        let stored = repo
            .find_active(&mut scope, tenant_id, "fake")
            .await
            .unwrap()
            .expect("the fake connection persisted");
        assert_eq!(stored.id, connection.id);
        assert_eq!(stored.provider, ProviderKind::Fake);
        scope.commit().await.unwrap();
    }

    // With snowflake IDs the connection id is no longer derived from the
    // idempotency key, so a sequential replay with the same key now
    // sees the existing active connection and is a 409 (the concurrent
    // same-key race is still folded via the insert-conflict adoption).
    let err = use_case
        .execute(start_command(tenant_id, key))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        sanvi_payments::PaymentsError::ConnectionExists(_)
    ));
    // …and a DIFFERENT key while an active connection exists is also
    // a 409-shaped conflict.
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
    let db = sanvi_testsupport::test_db().await;
    let migrator_pool = db.migrator_pool().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "fake-kind").await;

    // The CHECK admits 'fake' — the whole point of the 09.1 migration.
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let fake_connection = ProviderConnection::pending(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        ProviderKind::Fake,
        "acct_fake_check".to_string(),
        Utc::now(),
    )
    .unwrap();
    let stored_id = fake_connection.id;
    {
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
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
            .bind(stored_id.as_i64())
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
        TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
        Arc::new(providers),
        Arc::new(SqlxProviderConnectionRepository::new()),
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
            sanvi_testsupport::test_ids(),
        )),
    );
    (use_case, provider)
}

#[tokio::test]
async fn concurrent_creates_with_the_same_key_yield_one_account_and_one_row() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM payments.provider_connections WHERE tenant_id = $1",
    )
    .bind(tenant_id.as_i64())
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    assert_eq!(connection.external_account_id, orphaned.external_account_id);

    // One row (RLS: the count runs in a tenant-scoped tx), and the fake
    // provider holds exactly one account for the adopted key.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM payments.provider_connections WHERE tenant_id = $1",
    )
    .bind(tenant_id.as_i64())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(rows, 1);
}

// ---------------------------------------------------------------------------
// 09.2: the account-session use case.
// ---------------------------------------------------------------------------

async fn pending_connection(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
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
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        ProviderKind::Fake,
        account.external_account_id.clone(),
        Utc::now(),
    )
    .unwrap();
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.insert(&mut scope, &connection).await.unwrap();
    scope.commit().await.unwrap();
    connection
}

#[tokio::test]
async fn minting_a_session_moves_pending_to_onboarding_and_writes_the_audit_entry() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "session").await;
    let (use_case, provider) = session_fixture(&pool);
    let connection = pending_connection(&pool, tenant_id, &provider).await;
    let actor = UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id"));

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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let stored = repo
        .find_by_id(&mut scope, tenant_id, connection.id)
        .await
        .unwrap()
        .expect("row present");

    // The audit entry: actor, tenant, IP — and never the secret. The
    // shared RLS policy admits the row's own tenant.
    let entry: (Option<i64>, Option<String>) = sqlx::query_as(
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
    assert_eq!(entry.0, Some(actor.as_i64()));
    assert_eq!(entry.1.as_deref(), Some("203.0.113.7"));
    assert!(
        !after.to_string().contains(&session.client_secret),
        "the session secret never lands in the audit trail"
    );
}

#[tokio::test]
async fn a_session_is_refused_for_a_disconnected_connection() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "disconnected").await;
    let (use_case, provider) = session_fixture(&pool);
    let connection = pending_connection(&pool, tenant_id, &provider).await;

    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut disconnected = connection.clone();
    disconnected.disconnect(Utc::now()).unwrap();
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.update(&mut scope, &disconnected).await.unwrap();
    scope.commit().await.unwrap();

    let err = use_case
        .execute(
            UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
            None,
            tenant_id,
            &disconnected,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        sanvi_payments::PaymentsError::ConnectionNotActive(_)
    ));
}

#[tokio::test]
async fn an_expired_session_secret_is_rejected() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "expired").await;
    let (use_case, provider) = session_fixture(&pool);
    let connection = pending_connection(&pool, tenant_id, &provider).await;

    provider.script(sanvi_payments::adapters::FakeScript {
        next_session_expired: true,
        ..Default::default()
    });
    let err = use_case
        .execute(
            UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
            None,
            tenant_id,
            &connection,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        sanvi_payments::PaymentsError::ProviderUnavailable(_)
    ));
}

#[tokio::test]
async fn creating_a_connection_writes_the_audit_entry() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "audit").await;
    let use_case = start_connection_use_case(&pool);
    let actor = UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id"));

    let mut command = start_command(tenant_id, Uuid::now_v7());
    command.actor = actor;
    command.ip = Some("198.51.100.9".parse().unwrap());
    let connection = use_case.execute(command).await.unwrap();

    // audit.entries carries a shared RLS policy (platform scope OR the
    // row's own tenant id), so the read runs in a tenant-scoped tx.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let entry: (Option<i64>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT actor_id, tenant_id, host(ip) FROM audit.entries
         WHERE action = 'payments.connection_created' AND resource_id = $1",
    )
    .bind(connection.id.to_string())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(entry.0, Some(actor.as_i64()));
    assert_eq!(entry.1, Some(tenant_id.as_i64()));
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
    let body: serde_json::Value = actix_test::read_body_json(response).await;
    // 403 with the feature named — never a bare 404 — so the frontend can
    // render its upgrade prompt. This is where the entitlement bites; reading
    // the catalog (`the_catalog_route_is_readable_without_the_entitlement`)
    // deliberately does not.
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("payments.stripe_connect")
    );
}

// ---------------------------------------------------------------------------
// 09.3: the Connect webhook transport — signature, dedupe, routing,
// out-of-order guard, DLQ and the reconciliation safety net.
// ---------------------------------------------------------------------------

use sanvi_payments::adapters::SqlxWebhookEventRepository;
use sanvi_payments::application::{SyncConnections, WebhookDispatcher, WebhookReceiver};
use sanvi_payments::domain::{ConnectionLifecycleStore, EventPublisher, WebhookEventRepository};

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
/// provider handle the test scripts. 09.4 wires the checkout store into
/// the dispatcher so the payment handlers are live; 09.6 adds the payout
/// projection, the payout/tax alerts (recorded), and the tax re-checker.
struct WebhookFixture {
    receiver: Arc<WebhookReceiver>,
    dispatcher: Arc<WebhookDispatcher>,
    sync: Arc<SyncConnections>,
    webhooks: Arc<dyn WebhookEventRepository>,
    checkouts: Arc<dyn CheckoutLifecycleStore>,
    tax_recheck: Arc<sanvi_payments::application::CheckTenantTaxCompliance>,
    tx: TxManager,
    provider: FakeProvider,
}

async fn webhook_fixture(pool: &sanvi_persistence::PgPool) -> WebhookFixture {
    let provider = FakeProvider::new();
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(provider.clone())).unwrap();
    let registry = Arc::new(providers);
    let webhooks: Arc<dyn WebhookEventRepository> = Arc::new(SqlxWebhookEventRepository::new());
    let connection_rows: Arc<dyn ProviderConnectionRepository> =
        Arc::new(SqlxProviderConnectionRepository::new());
    let lifecycle: Arc<dyn ConnectionLifecycleStore> =
        Arc::new(SqlxProviderConnectionRepository::new());
    let events: Arc<dyn EventPublisher> =
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let tax_settings: Arc<dyn sanvi_payments::domain::TaxSettingsRepository> =
        Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new());
    let tax_recheck = Arc::new(sanvi_payments::application::CheckTenantTaxCompliance::new(
        tx_manager_for_pool(pool),
        connection_rows.clone(),
        registry.clone(),
        tax_settings,
        events.clone(),
    ));
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let receiver = Arc::new(WebhookReceiver::new(
        tx.clone(),
        webhooks.clone(),
        // The same adapter family the composition root wires: here the
        // fake's secret stands in for Stripe's.
        Arc::new(provider.clone()),
        vec!["127.0.0.1".to_string(), "::1".to_string()],
    ));
    let checkouts: Arc<dyn CheckoutLifecycleStore> = Arc::new(SqlxCheckoutRepository::new());
    let dispatcher = Arc::new(
        WebhookDispatcher::new(
            tx.clone(),
            webhooks.clone(),
            lifecycle.clone(),
            registry.clone(),
            events.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )
        .with_checkout_store(checkouts.clone())
        .with_refund_store(Arc::new(SqlxRefundRepository::new()))
        .with_dispute_store(Arc::new(SqlxDisputeRepository::new()))
        .with_payout_store(Arc::new(
            sanvi_payments::adapters::SqlxPayoutRepository::new(),
        ))
        .with_tax_checker(tax_recheck.clone()),
    );
    let sync = Arc::new(SyncConnections::new(
        tx.clone(),
        lifecycle,
        registry,
        events,
        chrono::Duration::zero(),
    ));
    WebhookFixture {
        receiver,
        dispatcher,
        sync,
        webhooks,
        checkouts,
        tax_recheck,
        tx,
        provider,
    }
}

fn tx_manager_for_pool(pool: &sanvi_persistence::PgPool) -> TxManager {
    TxManager::new(pool.clone(), sanvi_testsupport::test_ids())
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "poison").await;
    let fixture = webhook_fixture(&pool).await;

    // A connection whose provider adapter is NOT in this registry: every
    // processing attempt fails — the poison.
    let poison_connection = ProviderConnection::pending(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        ProviderKind::StripeConnect,
        "acct_poison".to_string(),
        Utc::now(),
    )
    .unwrap();
    {
        let repo = SqlxProviderConnectionRepository::new();
        let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
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
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        TenantId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
        let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
        repo.update(&mut scope, &synced).await.unwrap();
        scope.commit().await.unwrap();
    }

    // The route with REAL middleware products and a REAL pool.
    let subject = Subject {
        user_id: UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
                    let registry = Arc::new(providers);
                    let connections: Arc<dyn sanvi_payments::domain::ProviderConnectionRepository> =
                        Arc::new(SqlxProviderConnectionRepository::new());
                    PaymentsState {
                        config: PaymentsConfig::default(),
                        metrics: Arc::new(PaymentsMetrics::new(&prometheus_registry)),
                        list_providers: Arc::new(ListProviders::new(Arc::new(
                            RegistryProviderCatalog::new(
                                registry.clone(),
                                RuntimeEnvironment::Production,
                            ),
                        ))),
                        registry,
                        tx: TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                        connections: connections.clone(),
                        create_connection: Arc::new(StartProviderConnection::new(
                            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                            Arc::new(ProviderRegistry::new()),
                            connections.clone(),
                            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
                            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(sanvi_testsupport::test_ids())),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
                        account_session: Arc::new(CreateAccountSession::new(
                            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                            Arc::new(ProviderRegistry::new()),
                            connections,
                            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(sanvi_testsupport::test_ids())),
                        )),
                        disconnect_connection: Arc::new(
                            sanvi_payments::application::DisconnectConnection::new(
                                TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                                Arc::new(SqlxProviderConnectionRepository::new()),
                                Arc::new(SqlxPaymentRepository::new()),
                                Arc::new(SqlxDisputeRepository::new()),
                                Arc::new(sanvi_payments::adapters::SqlxPayoutRepository::new()),
                                Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
                                Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(sanvi_testsupport::test_ids())),
                            ),
                        ),
                        webhook_receiver: Arc::new(WebhookReceiver::new(
                            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                            Arc::new(SqlxWebhookEventRepository::new()),
                            Arc::new(FakeProvider::new()),
                            vec!["127.0.0.1".to_string()],
                        )),
                        checkouts: Arc::new(SqlxCheckoutRepository::new()),
                        create_checkout: Arc::new(CreateCheckout::new(
                            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                            Arc::new(ProviderRegistry::new()),
                            Arc::new(SqlxProviderConnectionRepository::new()),
                            Arc::new(SqlxCheckoutRepository::new()),
                            Arc::new(AllowShopTest),
                            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
                            Arc::new(TestFeePolicy(false)),
                            Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new()),
                            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
                        )),
                        checkout_throttle: Arc::new(FixedWindowThrottle::new(1000)),
                        payments: Arc::new(SqlxPaymentRepository::new()),
                        refunds: Arc::new(SqlxRefundRepository::new()),
                        disputes: Arc::new(SqlxDisputeRepository::new()),
                        list_payments: Arc::new(sanvi_payments::application::ListPayments::new(
                            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                            Arc::new(SqlxPaymentRepository::new()),
                        )),
                        get_payment: Arc::new(sanvi_payments::application::GetPaymentDetail::new(
                            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                            Arc::new(SqlxPaymentRepository::new()),
                            Arc::new(SqlxRefundRepository::new()),
                            Arc::new(SqlxDisputeRepository::new()),
                            Arc::new(SqlxCheckoutRepository::new()),
                            Arc::new(SqlxProviderConnectionRepository::new()),
                        )),
                        issue_refund: Arc::new(sanvi_payments::application::IssueRefund::new(
                            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                            Arc::new(ProviderRegistry::new()),
                            Arc::new(SqlxPaymentRepository::new()),
                            Arc::new(SqlxRefundRepository::new()),
                            Arc::new(SqlxProviderConnectionRepository::new()),
                            Arc::new(SqlxCheckoutRepository::new()),
                            Arc::new(sanvi_payments::adapters::OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
                            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(sanvi_testsupport::test_ids())),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
                        list_disputes: Arc::new(sanvi_payments::application::ListDisputes::new(
                            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                            Arc::new(SqlxDisputeRepository::new()),
                            Arc::new(SqlxPaymentRepository::new()),
                            Arc::new(SqlxCheckoutRepository::new()),
                            Arc::new(SqlxProviderConnectionRepository::new()),
                        )),
                        export_payments: Arc::new(
                            sanvi_payments::application::ExportPayments::new(
                                TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                                Arc::new(SqlxPaymentRepository::new()),
                                Arc::new(SqlxRefundRepository::new()),
                                Arc::new(SqlxDisputeRepository::new()),
                                Arc::new(SqlxCheckoutRepository::new()),
                                Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(sanvi_testsupport::test_ids())),
                            ),
                        ),
                        payouts: Arc::new(sanvi_payments::adapters::SqlxPayoutRepository::new()),
                        list_payouts: Arc::new(
                            sanvi_payments::application::ListPayouts::new(
                                TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                                Arc::new(SqlxProviderConnectionRepository::new()),
                                Arc::new(ProviderRegistry::new()),
                            ),
                        ),
                        tax_settings: Arc::new(
                            sanvi_payments::adapters::SqlxTaxSettingsRepository::new(),
                        ),
                        manage_tax_settings: Arc::new(
                            sanvi_payments::application::ManageTaxSettings::new(
                                TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
                                Arc::new(SqlxProviderConnectionRepository::new()),
                                Arc::new(ProviderRegistry::new()),
                                Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new()),
                            ),
                        ),
                        fee_store: Arc::new(NoFeeStore),
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

// ---------------------------------------------------------------------------
// 09.4: checkout on the connected account — direct charges, idempotent
// creation, the verified-domain allow-list, and webhook-driven truth.
// ---------------------------------------------------------------------------

use sanvi_payments::adapters::SqlxWebhookEventRepository as WebhookEventsRepo;
use sanvi_payments::application::conversion_event_id;
use sanvi_payments::domain::{CheckoutLifecycleStore, CheckoutRepository};

/// A full payments state on a real pool, sharing one fake-provider handle
/// with the test (for scripting) — the checkout slice's composition root.
fn checkout_fixture(
    pool: &sanvi_persistence::PgPool,
    provider: FakeProvider,
    throttle_limit: u32,
    fee_enabled: bool,
) -> PaymentsState {
    let prometheus_registry = prometheus::Registry::new();
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(provider.clone())).unwrap();
    let providers = Arc::new(providers);
    let connections: Arc<dyn ProviderConnectionRepository> =
        Arc::new(SqlxProviderConnectionRepository::new());
    let checkouts: Arc<dyn CheckoutRepository> = Arc::new(SqlxCheckoutRepository::new());
    let tax_settings_repo: Arc<dyn sanvi_payments::domain::TaxSettingsRepository> =
        Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new());
    PaymentsState {
        config: PaymentsConfig::default(),
        metrics: Arc::new(PaymentsMetrics::new(&prometheus_registry)),
        list_providers: Arc::new(ListProviders::new(Arc::new(RegistryProviderCatalog::new(
            providers.clone(),
            RuntimeEnvironment::NonProduction,
        )))),
        registry: providers.clone(),
        tx: TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
        connections: connections.clone(),
        create_connection: Arc::new(StartProviderConnection::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            providers.clone(),
            connections.clone(),
            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
                sanvi_testsupport::test_ids(),
            )),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        account_session: Arc::new(CreateAccountSession::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            providers.clone(),
            connections.clone(),
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
                sanvi_testsupport::test_ids(),
            )),
        )),
        disconnect_connection: Arc::new(sanvi_payments::application::DisconnectConnection::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            connections.clone(),
            Arc::new(SqlxPaymentRepository::new()),
            Arc::new(SqlxDisputeRepository::new()),
            Arc::new(sanvi_payments::adapters::SqlxPayoutRepository::new()),
            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
                sanvi_testsupport::test_ids(),
            )),
        )),
        webhook_receiver: Arc::new(WebhookReceiver::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            Arc::new(WebhookEventsRepo::new()),
            Arc::new(provider.clone()),
            vec!["127.0.0.1".to_string()],
        )),
        checkouts: checkouts.clone(),
        create_checkout: Arc::new(CreateCheckout::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            providers.clone(),
            connections.clone(),
            checkouts,
            Arc::new(AllowShopTest),
            Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
            Arc::new(TestFeePolicy(fee_enabled)),
            Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new()),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        checkout_throttle: Arc::new(FixedWindowThrottle::new(throttle_limit)),
        payments: Arc::new(SqlxPaymentRepository::new()),
        refunds: Arc::new(SqlxRefundRepository::new()),
        disputes: Arc::new(SqlxDisputeRepository::new()),
        list_payments: Arc::new(sanvi_payments::application::ListPayments::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            Arc::new(SqlxPaymentRepository::new()),
        )),
        get_payment: Arc::new(sanvi_payments::application::GetPaymentDetail::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            Arc::new(SqlxPaymentRepository::new()),
            Arc::new(SqlxRefundRepository::new()),
            Arc::new(SqlxDisputeRepository::new()),
            Arc::new(SqlxCheckoutRepository::new()),
            Arc::new(SqlxProviderConnectionRepository::new()),
        )),
        issue_refund: Arc::new(sanvi_payments::application::IssueRefund::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            providers.clone(),
            Arc::new(SqlxPaymentRepository::new()),
            Arc::new(SqlxRefundRepository::new()),
            Arc::new(SqlxProviderConnectionRepository::new()),
            Arc::new(SqlxCheckoutRepository::new()),
            Arc::new(sanvi_payments::adapters::OutboxEventPublisher::new(
                sanvi_testsupport::test_ids(),
            )),
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
                sanvi_testsupport::test_ids(),
            )),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        list_disputes: Arc::new(sanvi_payments::application::ListDisputes::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            Arc::new(SqlxDisputeRepository::new()),
            Arc::new(SqlxPaymentRepository::new()),
            Arc::new(SqlxCheckoutRepository::new()),
            Arc::new(SqlxProviderConnectionRepository::new()),
        )),
        export_payments: Arc::new(sanvi_payments::application::ExportPayments::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            Arc::new(SqlxPaymentRepository::new()),
            Arc::new(SqlxRefundRepository::new()),
            Arc::new(SqlxDisputeRepository::new()),
            Arc::new(SqlxCheckoutRepository::new()),
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
                sanvi_testsupport::test_ids(),
            )),
        )),
        payouts: Arc::new(sanvi_payments::adapters::SqlxPayoutRepository::new()),
        list_payouts: Arc::new(sanvi_payments::application::ListPayouts::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            connections.clone(),
            providers.clone(),
        )),
        tax_settings: tax_settings_repo.clone(),
        manage_tax_settings: Arc::new(sanvi_payments::application::ManageTaxSettings::new(
            TxManager::new(pool.clone(), sanvi_testsupport::test_ids()),
            connections.clone(),
            providers.clone(),
            tax_settings_repo.clone(),
        )),
        fee_store: Arc::new(NoFeeStore),
    }
}

/// The tenant app with REAL middleware products on a real pool.
async fn checkout_app(
    state: PaymentsState,
    tenant_id: TenantId,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse,
    Error = actix_web::Error,
> {
    let subject = Subject {
        user_id: UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        session_id: "sess_test".to_string(),
        aal: "aal2".to_string(),
        methods: vec!["password".to_string()],
        tenant_id: Some(tenant_id),
        role_ids: vec![],
        authenticated_at: Some(Utc::now()),
    };
    let tenant_context = TenantContext {
        tenant_id,
        slug: "checkout".to_string(),
        display_name: "Checkout".to_string(),
        status: sanvi_http::tenancy::TenantRuntimeStatus::Active,
        region: "jp".to_string(),
        default_locale: "en".to_string(),
        resolution_source: sanvi_http::tenancy::ResolutionSource::Subdomain,
    };
    let permission_gate: Arc<dyn PermissionGate> = Arc::new(FakePermissionGate { allowed: true });
    let feature_gate: Arc<dyn FeatureGate> = Arc::new(FakeFeatureGate { enabled: true });
    actix_test::init_service(
        App::new()
            .app_data(web::Data::new(state))
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
    .await
}

/// Drive a connection all the way to `Active` through the *legal*
/// lifecycle edges (Pending → Onboarding → Active) so checkout may exist
/// against it. Idempotent: a tenant that already has a live connection
/// gets it back, so tests can seed several payments against one setup.
async fn activate_connection(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    provider: &FakeProvider,
) -> ProviderConnection {
    let repo = SqlxProviderConnectionRepository::new();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    {
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
        if let Some(live) = repo
            .find_active(&mut scope, tenant_id, "fake")
            .await
            .unwrap()
        {
            scope.rollback().await.unwrap();
            return live;
        }
    }

    let connection = pending_connection(pool, tenant_id, provider).await;

    // First edge: onboarding begins.
    let early = provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    let mut conn = connection.clone();
    conn.apply_status(&early, ConnectionStatus::Onboarding, Utc::now())
        .unwrap();

    // Second edge: onboarding completes with the capability active.
    provider.approve_onboarding(&connection.external_account_id);
    let ready = provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    conn.apply_status(&ready, ConnectionStatus::Active, Utc::now())
        .unwrap();

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.update(&mut scope, &conn).await.unwrap();
    scope.commit().await.unwrap();
    conn
}

fn checkout_body(reference: &str, amount_minor: i64, currency: &str) -> serde_json::Value {
    serde_json::json!({
        "reference": reference,
        "amount_minor": amount_minor,
        "currency": currency,
        "customer_ref": {"customer": "c-1"},
        "locale": "ja",
        "success_url": "https://shop.test/thanks",
        "cancel_url": "https://shop.test/cart"
    })
}

#[tokio::test]
async fn checkout_key_rejects_changed_order_amount_customer_or_redirects() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "checkout-payload").await;
    let provider = FakeProvider::new();
    activate_connection(&pool, tenant_id, &provider).await;
    let app = checkout_app(checkout_fixture(&pool, provider, 1000, false), tenant_id).await;
    let key = Uuid::now_v7();
    let original = checkout_body("original-order", 2000, "JPY");
    let first = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(&original)
            .to_request(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    for (field, value) in [
        ("reference", serde_json::json!("other-order")),
        ("amount_minor", serde_json::json!(3000)),
        ("currency", serde_json::json!("USD")),
        ("customer_ref", serde_json::json!({"customer": "c-2"})),
        ("success_url", serde_json::json!("https://shop.test/other")),
        (
            "cancel_url",
            serde_json::json!("https://shop.test/other-cart"),
        ),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/v1/tenant/checkout")
                .insert_header(("Idempotency-Key", key.to_string()))
                .set_json(changed)
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "changed {field}");
    }
}

#[tokio::test]
async fn a_double_click_creates_one_checkout_and_a_replayed_key_returns_the_same_session() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "double-click").await;
    let provider = FakeProvider::new();
    activate_connection(&pool, tenant_id, &provider).await;

    let state = checkout_fixture(&pool, provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;
    let key = Uuid::now_v7();

    let first = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(checkout_body("order-1", 2000, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let created: serde_json::Value = actix_test::read_body_json(first).await;
    assert_eq!(created["status"], "pending");
    assert_eq!(created["amount_minor"], 2000, "JPY minor units end to end");
    assert_eq!(created["currency"], "JPY");
    assert_eq!(created["conversion_event_id"], serde_json::Value::Null);

    // The double-click replays the SAME key…
    let second = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(checkout_body("order-1", 2000, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let replayed: serde_json::Value = actix_test::read_body_json(second).await;
    assert_eq!(replayed["id"], created["id"], "same key → same checkout");
    assert_eq!(replayed["url"], created["url"], "same hosted session");

    // …and exactly one row and one event landed.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM payments.checkouts WHERE idempotency_key = $1")
            .bind(key)
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    let stored_status: String =
        sqlx::query_scalar("SELECT status FROM payments.checkouts WHERE idempotency_key = $1")
            .bind(key)
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(rows, 1);
    assert_eq!(stored_status, "open", "the session opened the checkout");

    // One buy button, one CheckoutCreated event.
    let outbox: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE event_type = 'payments.checkout_created'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outbox, 1);

    // The fake provider holds exactly one live session for it —
    // retrieval reused the same session instead of minting another.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_unscoped("t", "tests").await.unwrap();
    let account: String =
        sqlx::query_scalar("SELECT external_account_id FROM payments.provider_connections LIMIT 1")
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    scope.rollback().await.unwrap();
    let session_id = created["url"]
        .as_str()
        .unwrap()
        .split('/')
        .nth(3)
        .unwrap()
        .to_string();
    provider
        .retrieve_checkout(&account, &session_id)
        .await
        .expect("the same session is retrievable — nothing was minted twice");
}

#[tokio::test]
async fn checkout_is_refused_while_the_connection_cannot_accept_payments() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "not-active").await;
    let provider = FakeProvider::new();
    // A connection exists but never finished onboarding.
    pending_connection(&pool, tenant_id, &provider).await;

    let app = checkout_app(
        checkout_fixture(&pool, provider.clone(), 1000, false),
        tenant_id,
    )
    .await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(checkout_body("order-2", 500, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = actix_test::read_body_json(response).await;
    assert!(
        body["type"]
            .as_str()
            .unwrap()
            .ends_with("payments/connection-not-active"),
        "{body}"
    );
}

#[tokio::test]
async fn a_redirect_off_the_verified_domain_allow_list_is_rejected() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "allow-list").await;
    let provider = FakeProvider::new();
    activate_connection(&pool, tenant_id, &provider).await;

    let app = checkout_app(
        checkout_fixture(&pool, provider.clone(), 1000, false),
        tenant_id,
    )
    .await;
    for evil in [
        serde_json::json!({
            "reference": "o", "amount_minor": 100, "currency": "JPY",
            "success_url": "https://evil.test/thanks", "cancel_url": "https://shop.test/cart"
        }),
        // Plaintext redirect targets are refused before the allow-list.
        serde_json::json!({
            "reference": "o", "amount_minor": 100, "currency": "JPY",
            "success_url": "http://shop.test/thanks", "cancel_url": "https://shop.test/cart"
        }),
        // Userinfo smuggling dies at URL parsing, not at DNS.
        serde_json::json!({
            "reference": "o", "amount_minor": 100, "currency": "JPY",
            "success_url": "https://evil.test@shop.test/thanks", "cancel_url": "https://shop.test/cart"
        }),
    ] {
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/v1/tenant/checkout")
                .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
                .set_json(evil)
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = actix_test::read_body_json(response).await;
        assert!(
            body["type"]
                .as_str()
                .unwrap()
                .contains("domain-not-allowed"),
            "{body}"
        );
    }

    // Nothing was persisted for any rejected attempt.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM payments.checkouts")
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn tenant_b_cannot_create_on_or_read_tenant_as_checkouts() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_a = seed_tenant(&pool, "alpha-buyer").await;
    let tenant_b = seed_tenant(&pool, "beta-buyer").await;
    let provider = FakeProvider::new();
    activate_connection(&pool, tenant_a, &provider).await;

    let b_app = checkout_app(
        checkout_fixture(&pool, provider.clone(), 1000, false),
        tenant_b,
    )
    .await;
    let a_app = checkout_app(
        checkout_fixture(&pool, provider.clone(), 1000, false),
        tenant_a,
    )
    .await;

    // Tenant B has no connection of its own: its buy request gets the
    // connection blocker — never tenant A's account (the use case loads
    // only the requesting tenant's connection through RLS, and the
    // outbound command carries that account id alone).
    let response = actix_test::call_service(
        &b_app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(checkout_body("sneaky", 100, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let b_rows: i64 = {
        let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
        let mut scope = tx.begin_for(tenant_b.as_i64()).await.unwrap();
        let rows = sqlx::query_scalar("SELECT count(*) FROM payments.checkouts")
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
        scope.rollback().await.unwrap();
        rows
    };
    assert_eq!(b_rows, 0);

    // Give A a real checkout…
    let created = actix_test::call_service(
        &a_app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(checkout_body("a-order", 300, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let view: serde_json::Value = actix_test::read_body_json(created).await;

    // …and B cannot read it even knowing the id (RLS door): 404, not
    // someone else's order state.
    let forbidden = actix_test::call_service(
        &b_app,
        actix_test::TestRequest::get()
            .uri(&format!(
                "/api/v1/tenant/checkout/{}",
                view["id"].as_str().unwrap()
            ))
            .to_request(),
    )
    .await;
    assert_eq!(
        forbidden.status(),
        StatusCode::NOT_FOUND,
        "RLS: another tenant's checkout does not exist"
    );
}

#[tokio::test]
async fn a_currency_the_account_cannot_settle_is_refused_before_stripe() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "currency").await;
    let provider = FakeProvider::new();
    activate_connection(&pool, tenant_id, &provider).await;

    let app = checkout_app(checkout_fixture(&pool, provider, 1000, false), tenant_id).await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(checkout_body("usd-order", 1999, "USD"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = actix_test::read_body_json(response).await;
    assert!(
        body["type"]
            .as_str()
            .unwrap()
            .contains("currency-unsupported"),
        "{body}"
    );
}

#[tokio::test]
async fn checkout_creation_flooding_is_throttled_per_tenant() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "throttled").await;
    let provider = FakeProvider::new();
    activate_connection(&pool, tenant_id, &provider).await;

    // Two per minute, per tenant bucket.
    let app = checkout_app(checkout_fixture(&pool, provider, 2, false), tenant_id).await;
    for i in 0..2 {
        let ok = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/v1/tenant/checkout")
                .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
                .set_json(checkout_body(&format!("o-{i}"), 100, "JPY"))
                .to_request(),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::CREATED, "request {i}");
    }
    let denied = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(checkout_body("o-3", 100, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: serde_json::Value = actix_test::read_body_json(denied).await;
    assert!(
        body["type"].as_str().unwrap().contains("rate-limited"),
        "{body}"
    );
}

#[tokio::test]
async fn missing_idempotency_key_is_a_bad_request_not_a_second_session() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "no-key").await;
    let provider = FakeProvider::new();
    activate_connection(&pool, tenant_id, &provider).await;

    let app = checkout_app(checkout_fixture(&pool, provider, 1000, false), tenant_id).await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .set_json(checkout_body("o", 100, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A signed `checkout.session.completed` for a Sanvi-created session.
fn session_completed_event(
    event_id: &str,
    account: &str,
    checkout_id: CheckoutId,
    tenant_id: TenantId,
    session_id: &str,
    payment_intent: Option<&str>,
    created: i64,
) -> serde_json::Value {
    let mut object = serde_json::json!({
        "id": session_id,
        "object": "checkout.session",
        "amount_total": 2000,
        "currency": "jpy",
        "client_reference_id": "order-1",
        "metadata": {
            "checkout_id": checkout_id.to_string(),
            "tenant_id": tenant_id.to_string()
        }
    });
    if let Some(pi) = payment_intent {
        object["payment_intent"] = serde_json::json!(pi);
    }
    serde_json::json!({
        "id": event_id,
        "type": "checkout.session.completed",
        "created": created,
        "account": account,
        "data": {"object": object}
    })
}

fn payment_intent_event(
    event_id: &str,
    event_type: &str,
    account: &str,
    checkout_id: CheckoutId,
    tenant_id: TenantId,
    pi_id: &str,
    failure_code: Option<&str>,
    created: i64,
) -> serde_json::Value {
    let mut object = serde_json::json!({
        "id": pi_id,
        "object": "payment_intent",
        "amount": 2000,
        "currency": "jpy",
        "metadata": {
            "checkout_id": checkout_id.to_string(),
            "tenant_id": tenant_id.to_string()
        }
    });
    if let Some(code) = failure_code {
        object["last_payment_error"] =
            serde_json::json!({"code": code, "message": "card declined"});
    }
    serde_json::json!({
        "id": event_id,
        "type": event_type,
        "created": created,
        "account": account,
        "data": {"object": object}
    })
}

/// Create one checkout through the use case, against the SAME fake
/// provider handle that owns the tenant's connection.
async fn create_checkout_via_use_case(
    pool: &sqlx::PgPool,
    provider: &FakeProvider,
    tenant_id: TenantId,
    reference: &str,
) -> (CheckoutId, String) {
    let state = checkout_fixture(pool, provider.clone(), 1000, false);
    let outcome = state
        .create_checkout
        .execute(NewCheckoutCommand {
            tenant_id,
            reference: reference.to_string(),
            amount_minor: 2000,
            currency: "JPY".to_string(),
            customer_ref: None,
            locale: Some("ja".to_string()),
            success_url: "https://shop.test/thanks".to_string(),
            cancel_url: "https://shop.test/cart".to_string(),
            idempotency_key: Uuid::now_v7(),
        })
        .await
        .unwrap();
    (
        outcome.checkout.id,
        outcome.checkout.external_session_id.unwrap(),
    )
}

/// The live connection row's account id (platform-scope read: money-webhook
/// routing runs cross-tenant by design).
async fn live_account_id(fixture: &WebhookFixture, tenant_id: TenantId) -> String {
    let mut scope = fixture.tx.begin_unscoped("t", "tests").await.unwrap();
    let acct: String = sqlx::query_scalar(
        "SELECT external_account_id FROM payments.provider_connections
         WHERE tenant_id = $1 AND disconnected_at IS NULL LIMIT 1",
    )
    .bind(tenant_id.as_i64())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    acct
}

#[tokio::test]
async fn the_webhook_is_the_fact_a_customer_who_never_returns_still_ends_paid() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "webhook-truth").await;
    let fixture = webhook_fixture(&pool).await;

    // One connection, activated through the legal lifecycle edges.
    let connection = pending_connection(&pool, tenant_id, &fixture.provider).await;
    let repo = SqlxProviderConnectionRepository::new();
    let mut conn = connection.clone();
    let early = fixture
        .provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    conn.apply_status(&early, ConnectionStatus::Onboarding, Utc::now())
        .unwrap();
    fixture
        .provider
        .approve_onboarding(&connection.external_account_id);
    let ready = fixture
        .provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    conn.apply_status(&ready, ConnectionStatus::Active, Utc::now())
        .unwrap();
    {
        let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
        repo.update(&mut scope, &conn).await.unwrap();
        scope.commit().await.unwrap();
    }

    // …and the checkout created against it.
    let (checkout_id, session_id) =
        create_checkout_via_use_case(&pool, &fixture.provider, tenant_id, "order-webhook").await;
    let account = live_account_id(&fixture, tenant_id).await;
    assert_eq!(account, connection.external_account_id);

    // The customer pays at Stripe and closes the tab — the redirect never
    // happens. The webhook arrives instead.
    let event = session_completed_event(
        "evt_paid_1",
        &account,
        checkout_id,
        tenant_id,
        &session_id,
        Some("pi_paid_1"),
        epoch(5),
    );
    assert!(fixture.deliver(&event).await.unwrap());
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1, "{report:?}");

    // The read model: one succeeded JPY payment at minor units…
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let row: (String, i64, String, Option<i64>) = sqlx::query_as(
        "SELECT status, amount_minor, currency, checkout_id FROM payments.payments
          WHERE external_payment_id = 'pi_paid_1'",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    // …the checkout completed with its timestamp…
    let stored = <SqlxCheckoutRepository as CheckoutRepository>::find_by_id(
        &SqlxCheckoutRepository::new(),
        &mut scope,
        tenant_id,
        checkout_id,
    )
    .await
    .unwrap()
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(row.0, "succeeded");
    assert_eq!(
        row.1, 2000,
        "\u{a5}2,000 IS 2000 \u{2014} no hidden \u{f7}100"
    );
    assert_eq!(row.2, "JPY");
    assert_eq!(row.3, Some(checkout_id.as_i64()));
    assert_eq!(stored.status.as_str(), "complete");
    assert!(stored.completed_at.is_some());

    // …and the conversion pipeline's event fired exactly once.
    assert_eq!(fixture.outbox_events("payments.payment_succeeded").await, 1);

    // Redelivery of the same event changes nothing.
    assert!(!fixture.deliver(&event).await.unwrap(), "deduped");
    fixture.dispatcher.dispatch(10).await.unwrap();

    // The sibling event (pi.succeeded) converges on the same row and adds
    // no second success event.
    assert!(
        fixture
            .deliver(&payment_intent_event(
                "evt_paid_2",
                "payment_intent.succeeded",
                &account,
                checkout_id,
                tenant_id,
                "pi_paid_1",
                None,
                epoch(6),
            ))
            .await
            .unwrap()
    );
    fixture.dispatcher.dispatch(10).await.unwrap();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let payments: i64 = sqlx::query_scalar("SELECT count(*) FROM payments.payments")
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(payments, 1, "sibling events upsert, never duplicate");
    assert_eq!(
        fixture.outbox_events("payments.payment_succeeded").await,
        1,
        "one paid order, one PaymentSucceeded"
    );
}

#[tokio::test]
async fn a_forged_return_url_yields_no_order_and_failures_expire_correctly() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "forged-return").await;
    let fixture = webhook_fixture(&pool).await;
    let connection = pending_connection(&pool, tenant_id, &fixture.provider).await;
    // Activate through the legal lifecycle edges so the buy path is open.
    let mut conn = connection.clone();
    let early = fixture
        .provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    conn.apply_status(&early, ConnectionStatus::Onboarding, Utc::now())
        .unwrap();
    fixture
        .provider
        .approve_onboarding(&connection.external_account_id);
    let ready = fixture
        .provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    conn.apply_status(&ready, ConnectionStatus::Active, Utc::now())
        .unwrap();
    {
        let repo = SqlxProviderConnectionRepository::new();
        let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
        repo.update(&mut scope, &conn).await.unwrap();
        scope.commit().await.unwrap();
    }
    let account = connection.external_account_id.clone();

    let (checkout_id, session_id) =
        create_checkout_via_use_case(&pool, &fixture.provider, tenant_id, "order-forge").await;

    // Someone crafts a return URL claiming success. No webhook ever
    // arrived: the checkout MUST still be open.
    let stored = fixture
        .checkouts
        .find_by_external_session(
            &mut fixture.tx.begin_unscoped("t", "tests").await.unwrap(),
            &session_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.status.as_str(),
        "open",
        "the redirect is a hint; it moves nothing"
    );
    assert_eq!(stored.id, checkout_id);

    // A real decline arrives: failed payment recorded, outbox event fired,
    // but the checkout stays live (retryable at the hosted page).
    assert!(
        fixture
            .deliver(&payment_intent_event(
                "evt_failed_1",
                "payment_intent.payment_failed",
                &account,
                checkout_id,
                tenant_id,
                "pi_declined_1",
                Some("card_declined"),
                epoch(7),
            ))
            .await
            .unwrap()
    );
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1, "{report:?}");

    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let (status, failure): (String, serde_json::Value) = sqlx::query_as(
        "SELECT status, failure FROM payments.payments WHERE external_payment_id = 'pi_declined_1'",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(status, "failed");
    assert_eq!(failure["code"], "card_declined");
    assert_eq!(fixture.outbox_events("payments.payment_failed").await, 1);
    let still_open = fixture
        .checkouts
        .find_by_external_session(
            &mut fixture.tx.begin_unscoped("t", "tests").await.unwrap(),
            &session_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still_open.status.as_str(), "open");

    // The customer gives up; the session expires at the provider.
    let expired = serde_json::json!({
        "id": "evt_expired_1",
        "type": "checkout.session.expired",
        "created": epoch(3600),
        "account": account,
        "data": {"object": {
            "id": session_id,
            "object": "checkout.session",
            "metadata": {
                "checkout_id": checkout_id.to_string(),
                "tenant_id": tenant_id.to_string()
            }
        }}
    });
    assert!(fixture.deliver(&expired).await.unwrap());
    fixture.dispatcher.dispatch(10).await.unwrap();
    let expired_row = fixture
        .checkouts
        .find_by_external_session(
            &mut fixture.tx.begin_unscoped("t", "tests").await.unwrap(),
            &session_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(expired_row.status.as_str(), "expired");

    // An unknown-account money event is alerted and acknowledged — never
    // provisioned, never dead-lettered.
    assert!(
        fixture
            .deliver(&session_completed_event(
                "evt_stranger_1",
                "acct_never_seen_money",
                CheckoutId::from_snowflake(
                    sanvi_testsupport::test_ids().next_id().expect("test id")
                ),
                tenant_id,
                "cs_unknown",
                None,
                epoch(10),
            ))
            .await
            .unwrap()
    );
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.failed, 0, "unknown accounts are not poison");
    let stranger_rows: i64 = {
        let mut scope = fixture.tx.begin_unscoped("t", "tests").await.unwrap();
        let n = sqlx::query_scalar(
            "SELECT count(*) FROM payments.payments WHERE external_payment_id LIKE 'cs_%'",
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
        scope.rollback().await.unwrap();
        n
    };
    assert_eq!(
        stranger_rows, 0,
        "nothing was written for a stranger's session"
    );
}

#[tokio::test]
async fn the_confirmation_view_is_webhook_truthful_and_the_conversion_id_is_stable() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "confirmation").await;
    let fixture = webhook_fixture(&pool).await;
    let connection = pending_connection(&pool, tenant_id, &fixture.provider).await;
    let repo = SqlxProviderConnectionRepository::new();
    let mut conn = connection.clone();
    let early = fixture
        .provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    conn.apply_status(&early, ConnectionStatus::Onboarding, Utc::now())
        .unwrap();
    fixture
        .provider
        .approve_onboarding(&connection.external_account_id);
    let ready = fixture
        .provider
        .fetch_status(&connection.external_account_id)
        .await
        .unwrap();
    conn.apply_status(&ready, ConnectionStatus::Active, Utc::now())
        .unwrap();
    {
        let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
        repo.update(&mut scope, &conn).await.unwrap();
        scope.commit().await.unwrap();
    }

    let app = checkout_app(
        checkout_fixture(&pool, fixture.provider.clone(), 1000, false),
        tenant_id,
    )
    .await;
    let key = Uuid::now_v7();
    let created = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(checkout_body("order-confirm", 2000, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let view: serde_json::Value = actix_test::read_body_json(created).await;
    let checkout_id: CheckoutId = view["id"].as_str().unwrap().parse().unwrap();
    let session_id: String = view["url"]
        .as_str()
        .unwrap()
        .split('/')
        .nth(3)
        .unwrap()
        .to_string();

    // Before any webhook: pending, hosted URL still attached, no
    // conversion id.
    let polled = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri(&format!("/api/v1/tenant/checkout/{checkout_id}"))
            .to_request(),
    )
    .await;
    assert_eq!(polled.status(), StatusCode::OK);
    let before: serde_json::Value = actix_test::read_body_json(polled).await;
    assert_eq!(before["status"], "pending");
    assert!(before["url"].as_str().is_some());
    assert_eq!(before["conversion_event_id"], serde_json::Value::Null);

    // Pay through the transport (in production: the worker process).
    let account = live_account_id(&fixture, tenant_id).await;
    assert!(
        fixture
            .deliver(&session_completed_event(
                "evt_confirm_1",
                &account,
                checkout_id,
                tenant_id,
                &session_id,
                Some("pi_confirm_1"),
                epoch(5),
            ))
            .await
            .unwrap()
    );
    fixture.dispatcher.dispatch(10).await.unwrap();

    let polled = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri(&format!("/api/v1/tenant/checkout/{checkout_id}"))
            .to_request(),
    )
    .await;
    assert_eq!(polled.status(), StatusCode::OK);
    let after: serde_json::Value = actix_test::read_body_json(polled).await;
    assert_eq!(after["status"], "paid", "{after}");
    // The URL is gone once terminal — the hosted page has nothing to say.
    assert_eq!(after["url"], serde_json::Value::Null);

    // The conversion id equals the server-side derivation from the payment
    // row, identical across refreshes.
    let expected_payment_id: PaymentId = {
        let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
        let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM payments.payments WHERE external_payment_id = 'pi_confirm_1'",
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
        scope.rollback().await.unwrap();
        PaymentId::try_from(id).unwrap()
    };
    assert_eq!(
        after["conversion_event_id"],
        serde_json::json!(conversion_event_id(tenant_id, expected_payment_id)),
        "derived once per order, stable across refreshes"
    );
    let again = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri(&format!("/api/v1/tenant/checkout/{checkout_id}"))
            .to_request(),
    )
    .await;
    let refreshed: serde_json::Value = actix_test::read_body_json(again).await;
    assert_eq!(
        refreshed["conversion_event_id"],
        after["conversion_event_id"]
    );
}

// ---------------------------------------------------------------------------
// 09.5: payments, refunds & disputes — the read models, the refund use
// case (idempotency + recovery), webhook reconciliation, and the CSV
// export. Each test names the acceptance criterion or review finding it
// pins.
// ---------------------------------------------------------------------------

/// One connection driven to Active, one checkout, one
/// `checkout.session.completed` dispatched — a succeeded JPY payment of
/// 2000 minor units in the read model under `pi_id`.
async fn seed_paid_payment(
    pool: &sqlx::PgPool,
    fixture: &WebhookFixture,
    tenant_id: TenantId,
    reference: &str,
    pi_id: &str,
) -> String {
    activate_connection(pool, tenant_id, &fixture.provider).await;
    let (_checkout_id, session_id) =
        create_checkout_via_use_case(pool, &fixture.provider, tenant_id, reference).await;
    let account = live_account_id(fixture, tenant_id).await;
    let event = session_completed_event(
        &format!("evt_{pi_id}"),
        &account,
        CheckoutId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        &session_id,
        Some(pi_id),
        epoch(5),
    );
    assert!(
        fixture.deliver(&event).await.unwrap(),
        "the payment event stores"
    );
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1, "{report:?}");
    account
}

fn refund_body(minor_units: i64, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "amount": {"minor_units": minor_units, "currency": "JPY"},
        "reason": reason,
    })
}

/// The payment row id for an external payment-intent id.
async fn payment_row_id(pool: &sqlx::PgPool, tenant_id: TenantId, pi_id: &str) -> PaymentId {
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let id: i64 =
        sqlx::query_scalar("SELECT id FROM payments.payments WHERE external_payment_id = $1")
            .bind(pi_id)
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    scope.rollback().await.unwrap();
    PaymentId::try_from(id).unwrap()
}

#[tokio::test]
async fn a_retried_refund_key_issues_exactly_one_provider_refund_and_replays_the_record() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refund-retry").await;
    let fixture = webhook_fixture(&pool).await;
    seed_paid_payment(&pool, &fixture, tenant_id, "order-retry", "pi_retry_1").await;

    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;
    let payment_id = payment_row_id(&pool, tenant_id, "pi_retry_1").await;
    let key = Uuid::now_v7();

    let first = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}/refund"))
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(refund_body(500, "duplicate order"))
            .to_request(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let issued: serde_json::Value = actix_test::read_body_json(first).await;
    assert_eq!(issued["status"], "succeeded");
    assert_eq!(issued["amount_minor"], 500);

    // The retry with the same key replays the SAME refund — never a second one.
    let retry = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}/refund"))
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(refund_body(500, "duplicate order"))
            .to_request(),
    )
    .await;
    assert_eq!(retry.status(), StatusCode::OK, "a replay is a 200");
    let replayed: serde_json::Value = actix_test::read_body_json(retry).await;
    assert_eq!(replayed["id"], issued["id"]);

    // Exactly one provider execution and one outbox event.
    assert_eq!(
        fixture.provider.refund_executions(),
        1,
        "the retried key must not move money twice at the provider"
    );
    assert_eq!(fixture.outbox_events("payments.refund_issued").await, 1);
    // Attribution is persisted with the refund before money moves, and the
    // final audit remains recoverable until the independent chain writer runs.
    let audit_tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut audit_scope = audit_tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let audit_state: (Option<serde_json::Value>, bool) = sqlx::query_as(
        "SELECT audit_request, audit_ready FROM payments.refunds WHERE idempotency_key = $1",
    )
    .bind(key)
    .fetch_one(&mut **audit_scope.tx_mut().as_inner())
    .await
    .unwrap();
    audit_scope.rollback().await.unwrap();
    let intent = audit_state.0.expect("durable refund audit request");
    assert_eq!(intent["after"]["reason"], "duplicate order");
    assert!(intent["actor_id"].is_string());
    assert!(audit_state.1, "successful refund has a durable ready audit");
}

/// Simulate a crash after the chain INSERT but before the delivery marker.
/// Both changes must roll back while the financial intent remains durable.
struct AppendThenFailAudit;

#[async_trait::async_trait]
impl sanvi_audit::domain::AuditWriter for AppendThenFailAudit {
    async fn append(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        data: &sanvi_audit::domain::AuditEntryData,
    ) -> Result<i64, sanvi_audit::domain::AuditError> {
        sanvi_audit::adapters::SqlxAuditWriter::new(sanvi_testsupport::test_ids())
            .append(scope, data)
            .await?;
        Err(sanvi_audit::domain::AuditError::Storage(
            "crash after audit insert".into(),
        ))
    }
}

fn refund_with_unavailable_audit(
    state: &PaymentsState,
) -> sanvi_payments::application::IssueRefund {
    sanvi_payments::application::IssueRefund::new(
        state.tx.clone(),
        state.registry.clone(),
        state.payments.clone(),
        state.refunds.clone(),
        state.connections.clone(),
        state.checkouts.clone(),
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
        Arc::new(AppendThenFailAudit),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    )
}

fn refund_command(
    tenant_id: TenantId,
    payment_id: PaymentId,
) -> sanvi_payments::application::RefundCommand {
    sanvi_payments::application::RefundCommand {
        tenant_id,
        payment_id,
        amount: Some(Money::new(500, "JPY").unwrap()),
        reason: "original authorized reason".into(),
        note: Some("original private note".into()),
        idempotency_key: Uuid::now_v7(),
        actor: UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap()),
        ip: Some("192.0.2.23".parse().unwrap()),
    }
}

async fn drain_and_assert_original_refund_audit(
    state: &PaymentsState,
    command: &sanvi_payments::application::RefundCommand,
) {
    let drainer = sanvi_payments::application::DrainRefundAudits::new(
        state.tx.clone(),
        state.refunds.clone(),
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(
            sanvi_testsupport::test_ids(),
        )),
    );
    assert_eq!(drainer.execute(100).await.unwrap(), 1);
    assert_eq!(
        drainer.execute(100).await.unwrap(),
        0,
        "delivery is idempotent"
    );
    let mut scope = state
        .tx
        .begin_unscoped("assert_refund_audit", "test")
        .await
        .unwrap();
    let entries: Vec<(Option<i64>, Option<serde_json::Value>, Option<std::net::IpAddr>)> = sqlx::query_as(
        "SELECT actor_id, after, ip FROM audit.entries WHERE tenant_id = $1 AND action = 'payments.refund_issued'",
    ).bind(command.tenant_id.as_i64()).fetch_all(&mut **scope.tx_mut().as_inner()).await.unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(
        entries.len(),
        1,
        "no orphan audit survives a failed delivery transaction"
    );
    assert_eq!(entries[0].0, Some(command.actor.as_i64()));
    assert_eq!(entries[0].2, command.ip);
    let detail = entries[0].1.as_ref().unwrap();
    assert_eq!(detail["reason"], "original authorized reason");
    assert_eq!(detail["note"], "original private note");
    assert_eq!(detail["status"], "succeeded");
    assert!(
        detail["external_refund_id"]
            .as_str()
            .unwrap()
            .starts_with("re_")
    );
}

#[tokio::test]
async fn refund_audit_delivery_failure_preserves_finalized_money_and_original_actor() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refund-audit-delivery").await;
    let fixture = webhook_fixture(&pool).await;
    seed_paid_payment(
        &pool,
        &fixture,
        tenant_id,
        "audit-delivery",
        "pi_audit_delivery",
    )
    .await;
    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let command = refund_command(
        tenant_id,
        payment_row_id(&pool, tenant_id, "pi_audit_delivery").await,
    );
    let (refund, _) = refund_with_unavailable_audit(&state)
        .execute(command.clone())
        .await
        .unwrap();
    assert_eq!(
        refund.status,
        sanvi_payments::domain::RefundStatus::Succeeded
    );
    let mut replay = command.clone();
    replay.actor = UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    replay.note = Some("retrying caller must not replace attribution".into());
    assert!(state.issue_refund.execute(replay).await.unwrap().1);
    drain_and_assert_original_refund_audit(&state, &command).await;
}

#[tokio::test]
async fn refund_legacy_pending_retry_keeps_original_actor_without_inventing_missing_context() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refund-legacy-audit").await;
    let fixture = webhook_fixture(&pool).await;
    seed_paid_payment(
        &pool,
        &fixture,
        tenant_id,
        "legacy-audit",
        "pi_legacy_audit",
    )
    .await;
    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let command = refund_command(
        tenant_id,
        payment_row_id(&pool, tenant_id, "pi_legacy_audit").await,
    );
    fixture.provider.script(FakeScript {
        fail_next_refund: Some("unavailable".into()),
        ..Default::default()
    });
    let issuer = refund_with_unavailable_audit(&state);
    assert!(issuer.execute(command.clone()).await.is_err());
    let mut scope = state.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query("UPDATE payments.refunds SET audit_request = NULL WHERE idempotency_key = $1")
        .bind(command.idempotency_key)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
    scope.commit().await.unwrap();
    let mut retry = command.clone();
    retry.actor = UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    retry.note = Some("new caller's unrelated note".into());
    issuer.execute(retry).await.unwrap();
    let mut scope = state.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let snapshot: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT audit_request FROM payments.refunds WHERE idempotency_key = $1")
            .bind(command.idempotency_key)
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    scope.rollback().await.unwrap();
    let snapshot = snapshot.expect("legacy pending request is attributed before a provider retry");
    assert_eq!(snapshot["actor_id"], serde_json::json!(command.actor));
    assert_eq!(snapshot["after"]["note"], serde_json::Value::Null);
    assert_eq!(snapshot["ip"], serde_json::Value::Null);
}

#[tokio::test]
async fn refund_provider_success_recovers_through_webhook_metadata_after_a_crash() {
    assert_refund_provider_success_recovery(true).await;
}

#[tokio::test]
async fn refund_webhook_winner_adopts_original_audit_before_local_row_is_deleted() {
    assert_refund_provider_success_recovery(false).await;
}

#[tokio::test]
async fn refund_api_finalization_and_correlated_webhook_emit_one_success_event() {
    assert_refund_event_transition_race(true).await;
}

#[tokio::test]
async fn concurrent_refund_webhook_workers_emit_one_success_event_and_preserve_success() {
    assert_refund_event_transition_race(false).await;
}

async fn assert_refund_event_transition_race(api_finalization: bool) {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refund-finalize-race").await;
    let fixture = webhook_fixture(&pool).await;
    let account = seed_paid_payment(
        &pool,
        &fixture,
        tenant_id,
        "finalize-race",
        "pi_finalize_race",
    )
    .await;
    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let command = refund_command(
        tenant_id,
        payment_row_id(&pool, tenant_id, "pi_finalize_race").await,
    );
    fixture.provider.script(FakeScript {
        fail_next_refund: Some("unavailable".into()),
        ..Default::default()
    });
    assert!(
        refund_with_unavailable_audit(&state)
            .execute(command.clone())
            .await
            .is_err()
    );
    let mut read = state.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let mut refund = state
        .refunds
        .find_by_idempotency_key(&mut read, tenant_id, command.idempotency_key)
        .await
        .unwrap()
        .unwrap();
    read.rollback().await.unwrap();
    let external = "re_finalize_race";
    let webhook = serde_json::json!({
        "id": "evt_finalize_race", "type": "refund.updated", "created": epoch(6), "account": account,
        "data": {"object": {"id": external, "payment_intent": "pi_finalize_race",
            "amount": 500, "currency": "jpy", "status": "succeeded",
            "metadata": {"sanvi_refund_id": refund.id, "sanvi_idempotency_key": command.idempotency_key}}}
    });
    fixture.deliver(&webhook).await.unwrap();

    // Hold either the API transition/outbox transaction or the local row while
    // two webhook workers start. Observe database lock waits before release.
    let mut finalizing = state.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    if api_finalization {
        assert!(
            state
                .refunds
                .find_by_external_id(&mut finalizing, tenant_id, external)
                .await
                .unwrap()
                .is_none()
        );
        refund.external_refund_id = Some(external.into());
        refund.status = sanvi_payments::domain::RefundStatus::Succeeded;
        assert!(
            state
                .refunds
                .try_finalize(&mut finalizing, tenant_id, &refund)
                .await
                .unwrap()
        );
        OutboxEventPublisher::new(sanvi_testsupport::test_ids())
            .publish_scoped(
                &mut finalizing,
                tenant_id,
                &[sanvi_payments::domain::PaymentsEvent::RefundIssued {
                    refund_id: refund.id,
                    payment_id: command.payment_id,
                    provider: "fake".into(),
                    amount: sanvi_payments::domain::events::EventAmount::new(&refund.amount),
                    reason: refund.reason.clone(),
                }],
            )
            .await
            .unwrap();
    } else {
        sqlx::query("SELECT id FROM payments.refunds WHERE id = $1 FOR UPDATE")
            .bind(refund.id.as_i64())
            .execute(&mut **finalizing.tx_mut().as_inner())
            .await
            .unwrap();
    }
    let workers = if api_finalization { 1 } else { 2 };
    let mut racing_webhooks = Vec::new();
    for _ in 0..workers {
        let dispatcher = fixture.dispatcher.clone();
        racing_webhooks.push(tokio::spawn(async move { dispatcher.dispatch(10).await }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database()
                 AND pid <> pg_backend_pid() AND wait_event_type = 'Lock'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if blocked >= workers {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("webhook reached the API's transaction lock");
    finalizing.commit().await.unwrap();
    for task in racing_webhooks {
        let report = task.await.unwrap().unwrap();
        assert_eq!(report.failed, 0, "{report:?}");
    }
    assert_eq!(
        fixture.outbox_events("payments.refund_issued").await,
        1,
        "one committed financial transition has one success event"
    );
    if !api_finalization {
        let mut late = webhook.clone();
        late["id"] = serde_json::json!("evt_late_pending_refund");
        late["data"]["object"]["status"] = serde_json::json!("pending");
        fixture.deliver(&late).await.unwrap();
        assert_eq!(fixture.dispatcher.dispatch(10).await.unwrap().failed, 0);
        let mut scope = state.tx.begin_for(tenant_id.as_i64()).await.unwrap();
        let stored = state
            .refunds
            .find_by_id(&mut scope, tenant_id, refund.id)
            .await
            .unwrap()
            .unwrap();
        scope.rollback().await.unwrap();
        assert_eq!(
            stored.status,
            sanvi_payments::domain::RefundStatus::Succeeded,
            "a stale pending webhook cannot undo terminal success"
        );
        late["id"] = serde_json::json!("evt_duplicate_success_refund");
        late["data"]["object"]["status"] = serde_json::json!("succeeded");
        fixture.deliver(&late).await.unwrap();
        assert_eq!(fixture.dispatcher.dispatch(10).await.unwrap().failed, 0);
        assert_eq!(fixture.outbox_events("payments.refund_issued").await, 1);
    }
}

async fn assert_refund_provider_success_recovery(correlated_webhook: bool) {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refund-audit-crash").await;
    let fixture = webhook_fixture(&pool).await;
    let account =
        seed_paid_payment(&pool, &fixture, tenant_id, "audit-crash", "pi_audit_crash").await;
    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let command = refund_command(
        tenant_id,
        payment_row_id(&pool, tenant_id, "pi_audit_crash").await,
    );
    fixture.provider.script(FakeScript {
        fail_next_refund: Some("first attempt unavailable".into()),
        ..Default::default()
    });
    let issuer = refund_with_unavailable_audit(&state);
    assert!(issuer.execute(command.clone()).await.is_err());
    let mut scope = state.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let pending = state
        .refunds
        .find_by_idempotency_key(&mut scope, tenant_id, command.idempotency_key)
        .await
        .unwrap()
        .unwrap();
    let snapshot: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT audit_request FROM payments.refunds WHERE id = $1")
            .bind(pending.id.as_i64())
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    assert_eq!(
        snapshot.unwrap()["actor_id"],
        serde_json::json!(command.actor)
    );
    scope.rollback().await.unwrap();

    // The provider succeeds; deliberately omit local finalization, as a killed
    // API process would. Only the signed webhook/retry can recover this result.
    let provider_result = fixture
        .provider
        .refund(&sanvi_payments::domain::ProviderRefundCommand {
            external_account_id: account.clone(),
            external_payment_id: "pi_audit_crash".into(),
            refund: pending.clone(),
            idempotency_key: command.idempotency_key,
        })
        .await
        .unwrap();
    let metadata = if correlated_webhook {
        serde_json::json!({"sanvi_refund_id": pending.id, "sanvi_idempotency_key": command.idempotency_key})
    } else {
        serde_json::json!({})
    };
    let webhook = serde_json::json!({
        "id": "evt_refund_audit_recover", "type": "refund.updated", "created": epoch(6), "account": account,
        "data": {"object": {"id": provider_result.external_refund_id, "payment_intent": "pi_audit_crash",
            "amount": 500, "currency": "jpy", "status": "succeeded", "metadata": metadata}}
    });
    fixture.deliver(&webhook).await.unwrap();
    let dispatched = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(dispatched.processed, 1, "{dispatched:?}");
    if !correlated_webhook {
        // Legacy webhook without correlation created a different winner. The
        // interrupted API retry must transfer the immutable audit before delete.
        let mut replay = command.clone();
        replay.actor = UserId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
        replay.note = Some("another caller".into());
        issuer.execute(replay).await.unwrap();
    }
    drain_and_assert_original_refund_audit(&state, &command).await;
    let mut scope = state.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let owner = state
        .refunds
        .find_by_idempotency_key(&mut scope, tenant_id, command.idempotency_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        owner.status,
        sanvi_payments::domain::RefundStatus::Succeeded
    );
    assert_eq!(owner.created_by, Some(command.actor));
    scope.rollback().await.unwrap();
}

#[tokio::test]
async fn a_failed_provider_call_marks_the_refund_failed_and_a_retried_key_redrives_it() {
    // Review finding: the old code left the row Pending forever after a
    // provider rejection — a phantom pending refund that owned its
    // idempotency key and never re-executed.
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refunde-fail").await;
    let fixture = webhook_fixture(&pool).await;
    seed_paid_payment(&pool, &fixture, tenant_id, "order-fail", "pi_fail_1").await;

    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;
    let payment_id = payment_row_id(&pool, tenant_id, "pi_fail_1").await;
    let key = Uuid::now_v7();

    fixture.provider.script(FakeScript {
        fail_next_refund: Some("insufficient_balance".to_string()),
        ..FakeScript::default()
    });

    let first = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}/refund"))
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(refund_body(500, "duplicate order"))
            .to_request(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::BAD_GATEWAY);

    // The row is honestly failed — no phantom pending.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let status: String =
        sqlx::query_scalar("SELECT status FROM payments.refunds WHERE idempotency_key = $1")
            .bind(key)
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(status, "failed");

    // The retried key RE-DRIVES: the failed attempt owns nothing, and the
    // provider-side idempotency keeps it to exactly one refund.
    let retry = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}/refund"))
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(refund_body(500, "duplicate order"))
            .to_request(),
    )
    .await;
    assert_eq!(
        retry.status(),
        StatusCode::CREATED,
        "a re-driven key issues the refund fresh"
    );
    let reissued: serde_json::Value = actix_test::read_body_json(retry).await;
    assert_eq!(reissued["status"], "succeeded");
    assert_eq!(
        fixture.provider.refund_executions(),
        2,
        "one failed attempt + one successful drive"
    );
    assert_eq!(
        fixture.outbox_events("payments.refund_issued").await,
        1,
        "exactly one RefundIssued across both attempts"
    );
}

#[tokio::test]
async fn partial_refunds_sum_to_the_total_and_an_over_refund_reports_the_remaining() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refund-partial").await;
    let fixture = webhook_fixture(&pool).await;
    seed_paid_payment(&pool, &fixture, tenant_id, "order-partial", "pi_partial_1").await;

    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;
    let payment_id = payment_row_id(&pool, tenant_id, "pi_partial_1").await;

    for minor in [500, 1500] {
        let res = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri(&format!("/api/v1/tenant/payments/{payment_id}/refund"))
                .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
                .set_json(refund_body(minor, "partial by line item"))
                .to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CREATED);
    }
    // The detail view's refunded sum now covers the full amount…
    let detail = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}"))
            .to_request(),
    )
    .await;
    assert_eq!(detail.status(), StatusCode::OK);
    let view: serde_json::Value = actix_test::read_body_json(detail).await;
    let refunded_sum: i64 = view["refunds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["amount_minor"].as_i64().unwrap())
        .sum();
    assert_eq!(refunded_sum, 2000);

    // …so any further refund is rejected WITH the remaining amount.
    let over = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}/refund"))
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(refund_body(100, "one too many"))
            .to_request(),
    )
    .await;
    assert_eq!(over.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = actix_test::read_body_json(over).await;
    let detail_text = body["detail"].as_str().unwrap();
    assert!(
        detail_text.contains("exceeds remaining") && detail_text.contains('0'),
        "over-refund reports remaining refundable 0: {detail_text}"
    );
}

#[tokio::test]
async fn a_replayed_key_answers_with_the_refund_even_after_the_total_is_consumed() {
    // Review finding: the over-refund guard ran BEFORE the idempotency
    // lookup, so replaying a fully-consumed refund answered 400 instead
    // of the record it already owed.
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refund-full").await;
    let fixture = webhook_fixture(&pool).await;
    seed_paid_payment(&pool, &fixture, tenant_id, "order-full", "pi_full_1").await;

    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;
    let payment_id = payment_row_id(&pool, tenant_id, "pi_full_1").await;
    let key = Uuid::now_v7();

    // A FULL refund (null amount = remaining) consumes the payment…
    let full = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}/refund"))
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(serde_json::json!({"amount": null, "reason": "chargeback"}))
            .to_request(),
    )
    .await;
    assert_eq!(full.status(), StatusCode::CREATED);
    let issued: serde_json::Value = actix_test::read_body_json(full).await;
    assert_eq!(issued["amount_minor"], 2000);

    // …and the replay still answers with that refund, not an over-refund error.
    let replay = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}/refund"))
            .insert_header(("Idempotency-Key", key.to_string()))
            .set_json(serde_json::json!({"amount": null, "reason": "chargeback"}))
            .to_request(),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::OK);
    let replayed: serde_json::Value = actix_test::read_body_json(replay).await;
    assert_eq!(replayed["id"], issued["id"]);
}

#[tokio::test]
async fn a_dashboard_refund_lands_in_the_read_model_without_manual_intervention() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "refund-dashboard").await;
    let fixture = webhook_fixture(&pool).await;
    seed_paid_payment(&pool, &fixture, tenant_id, "order-dash", "pi_dash_1").await;

    // The tenant refunds from Stripe's Dashboard: only `charge.refunded`
    // arrives, naming the charge AND its payment intent.
    let charge_event = serde_json::json!({
        "id": "evt_dash_refund",
        "type": "charge.refunded",
        "created": epoch(6),
        "account": live_account_id(&fixture, tenant_id).await,
        "data": {"object": {
            "id": "ch_dash_1",
            "object": "charge",
            "payment_intent": "pi_dash_1",
            "refunds": {"data": [{
                "id": "re_dash_1",
                "object": "refund",
                "amount": 800,
                "currency": "jpy",
                "status": "succeeded",
                "created": epoch(6)
            }]}
        }}
    });
    assert!(fixture.deliver(&charge_event).await.unwrap());
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1, "{report:?}");

    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let row: (i64, String, String) = sqlx::query_as(
        "SELECT r.amount_minor, r.currency, r.status
         FROM payments.refunds r
         JOIN payments.payments p ON p.id = r.payment_id
         WHERE r.external_refund_id = 're_dash_1' AND p.external_payment_id = 'pi_dash_1'",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(row, (800, "JPY".to_string(), "succeeded".to_string()));
    assert_eq!(fixture.outbox_events("payments.refund_issued").await, 1);

    // Redelivery stays idempotent: the transport dedupes the event and
    // the dispatcher finds nothing new.
    assert!(
        !fixture.deliver(&charge_event).await.unwrap(),
        "the redelivery is deduped at the transport"
    );
    let again = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(again.processed, 0, "nothing new to process");
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_unscoped("t", "tests").await.unwrap();
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM payments.refunds WHERE external_refund_id = 're_dash_1'",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(rows, 1, "one row per provider refund id");
}

#[tokio::test]
async fn a_dispute_lifecycle_routes_by_payment_intent_shows_disputed_then_closed() {
    // Review finding: disputes routed ONLY by charge id while payments are
    // keyed by payment intent — every real dispute was silently dropped.
    // The fixture carries a charge id that matches nothing, forcing the
    // payment_intent route.
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "disputes").await;
    let fixture = webhook_fixture(&pool).await;
    seed_paid_payment(&pool, &fixture, tenant_id, "order-dispute", "pi_dispute_1").await;
    let account = live_account_id(&fixture, tenant_id).await;
    let payment_id = payment_row_id(&pool, tenant_id, "pi_dispute_1").await;

    let opened = serde_json::json!({
        "id": "evt_dp_open",
        "type": "charge.dispute.created",
        "created": epoch(7),
        "account": account,
        "data": {"object": {
            "id": "dp_test_1",
            "object": "dispute",
            "amount": 2000,
            "currency": "jpy",
            "charge": "ch_no_such_charge",
            "payment_intent": "pi_dispute_1",
            "status": "needs_response",
            "reason": "fraudulent",
            "evidence_details": {"due_by": epoch(86_400)}
        }}
    });
    assert!(fixture.deliver(&opened).await.unwrap());
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1, "{report:?}");
    assert_eq!(fixture.outbox_events("payments.dispute_opened").await, 1);

    // The state machine moves on without Sanvi: the tenant responds at
    // Stripe and wins; `charge.dispute.closed` lands.
    let closed = serde_json::json!({
        "id": "evt_dp_close",
        "type": "charge.dispute.closed",
        "created": epoch(8),
        "account": account,
        "data": {"object": {
            "id": "dp_test_1",
            "object": "dispute",
            "amount": 2000,
            "currency": "jpy",
            "charge": "ch_no_such_charge",
            "payment_intent": "pi_dispute_1",
            "status": "won"
        }}
    });
    assert!(fixture.deliver(&closed).await.unwrap());
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1, "{report:?}");

    // The detail view tells the whole story with deep links.
    let app = checkout_app(
        checkout_fixture(&pool, fixture.provider.clone(), 1000, false),
        tenant_id,
    )
    .await;
    let detail = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri(&format!("/api/v1/tenant/payments/{payment_id}"))
            .to_request(),
    )
    .await;
    assert_eq!(detail.status(), StatusCode::OK);
    let view: serde_json::Value = actix_test::read_body_json(detail).await;
    assert_eq!(view["disputes"][0]["status"], "won");
    let url = view["disputes"][0]["dashboard_url"].as_str().unwrap();
    assert!(url.contains("dashboard.stripe.com"), "{url}");
    assert!(url.contains("dp_test_1"));
    let kinds: Vec<&str> = view["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"disputed") && kinds.contains(&"dispute_closed"),
        "timeline holds both facts in order: {kinds:?}"
    );

    // And the disputes list renders the link too.
    let list = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/payments/disputes")
            .to_request(),
    )
    .await;
    assert_eq!(list.status(), StatusCode::OK);
    let list_view: serde_json::Value = actix_test::read_body_json(list).await;
    assert_eq!(list_view["items"][0]["external_dispute_id"], "dp_test_1");
    assert!(
        list_view["items"][0]["dashboard_url"]
            .as_str()
            .unwrap()
            .contains("acct"),
        "the deep link anchors on the connected account"
    );
}

#[tokio::test]
async fn the_csv_export_is_streamed_tenant_scoped_and_audited() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();

    // Two tenants, one succeeded payment each — the export must leak
    // neither way. One shared provider fixture: account ids are unique
    // platform-wide, and clones of the fake share its mint.
    let fixture = webhook_fixture(&pool).await;
    let mut seeded = Vec::new();
    for slug in ["export-a", "export-b"] {
        let tenant_id = seed_tenant(&pool, slug).await;
        let pi = format!("pi_{}", slug.replace('-', "_"));
        seed_paid_payment(&pool, &fixture, tenant_id, &format!("order-{slug}"), &pi).await;
        let payment_id = payment_row_id(&pool, tenant_id, &pi).await;
        seeded.push((tenant_id, payment_id));
    }

    for (tenant_id, own_id) in &seeded {
        let app = checkout_app(
            checkout_fixture(&pool, fixture.provider.clone(), 1000, false),
            *tenant_id,
        )
        .await;
        let res = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri("/api/v1/tenant/payments/export")
                .to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/csv; charset=utf-8"),
            "the streaming body is CSV"
        );
        let body = actix_test::read_body(res).await;
        let csv_text = std::str::from_utf8(&body).unwrap();
        // The fixed header is the contract.
        assert!(
            csv_text.starts_with(
                "payment_id,external_payment_id,checkout_reference,amount_minor,currency,status,fee_minor,fee_currency,captured_at,created_at,refunded_minor,dispute_status"
            ),
            "{csv_text}"
        );
        // Exactly one data row and it is this tenant's — a cross-tenant
        // probe returns nothing foreign.
        let rows: Vec<&str> = csv_text
            .lines()
            .skip(1)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains(&own_id.to_string()), "{}", rows[0]);
    }

    // Both exports audited: actor + filters, visible to each own tenant.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(seeded[0].0.as_i64()).await.unwrap();
    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT after FROM audit.entries WHERE action = 'payments.export' LIMIT 1",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(
        after["streamed"], true,
        "the export is recorded as streamed"
    );
    assert!(after["filters"].is_object());
}

#[tokio::test]
async fn the_payments_list_serves_every_filter_and_walks_pages_without_loss_or_duplication() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "list-filters").await;
    let fixture = webhook_fixture(&pool).await;

    // Three succeeded payments…
    for i in 1..=3 {
        seed_paid_payment(
            &pool,
            &fixture,
            tenant_id,
            &format!("order-list-{i}"),
            &format!("pi_list_{i}"),
        )
        .await;
    }
    // …plus one failed one, straight from the provider's truth.
    activate_connection(&pool, tenant_id, &fixture.provider).await;
    let (checkout_id, _session_id) =
        create_checkout_via_use_case(&pool, &fixture.provider, tenant_id, "order-list-dead").await;
    let account = live_account_id(&fixture, tenant_id).await;
    let failed_event = payment_intent_event(
        "evt_failed",
        "payment_intent.payment_failed",
        &account,
        checkout_id,
        tenant_id,
        "pi_list_dead",
        Some("card_declined"),
        epoch(6),
    );
    assert!(fixture.deliver(&failed_event).await.unwrap());
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1);

    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;

    // Walk every page with limit=2: no loss, no duplication.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let uri = match &cursor {
            None => "/api/v1/tenant/payments?limit=2".to_string(),
            Some(c) => format!("/api/v1/tenant/payments?limit=2&cursor={c}"),
        };
        let res =
            actix_test::call_service(&app, actix_test::TestRequest::get().uri(&uri).to_request())
                .await;
        assert_eq!(res.status(), StatusCode::OK);
        let view: serde_json::Value = actix_test::read_body_json(res).await;
        for item in view["items"].as_array().unwrap() {
            seen.push(item["id"].as_str().unwrap().to_string());
        }
        match view["next_cursor"].as_str() {
            Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
            _ => break,
        }
    }
    seen.sort();
    seen.dedup();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let expected: i64 =
        sqlx::query_scalar("SELECT count(*) FROM payments.payments WHERE tenant_id = $1")
            .bind(tenant_id.as_i64())
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(expected, 4);
    assert_eq!(seen.len(), 4, "the walk covers every row exactly once");

    // Status filter serves exactly the matching subset; an unknown status
    // is a 400, not a silent empty page.
    let res = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/payments?status=succeeded")
            .to_request(),
    )
    .await;
    let view: serde_json::Value = actix_test::read_body_json(res).await;
    assert_eq!(view["items"].as_array().unwrap().len(), 3);

    let res = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/payments?status=succeded")
            .to_request(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Currency filter: USD exists in no row here.
    let res = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/payments?currency=USD")
            .to_request(),
    )
    .await;
    let view: serde_json::Value = actix_test::read_body_json(res).await;
    assert!(view["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn the_payments_list_and_export_filter_by_payout_status() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "payout-status-filters").await;
    let fixture = webhook_fixture(&pool).await;

    // Seed 4 payments: pending, paid, failed, and NULL payout status.
    seed_paid_payment(
        &pool,
        &fixture,
        tenant_id,
        "order-po-pending",
        "pi_po_pending",
    )
    .await;
    seed_paid_payment(&pool, &fixture, tenant_id, "order-po-paid", "pi_po_paid").await;
    seed_paid_payment(
        &pool,
        &fixture,
        tenant_id,
        "order-po-failed",
        "pi_po_failed",
    )
    .await;
    seed_paid_payment(&pool, &fixture, tenant_id, "order-po-none", "pi_po_none").await;

    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query(
        "UPDATE payments.payments SET payout_status = 'pending' WHERE external_payment_id = 'pi_po_pending' AND tenant_id = $1",
    )
    .bind(tenant_id.as_i64())
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();

    sqlx::query(
        "UPDATE payments.payments SET payout_status = 'paid' WHERE external_payment_id = 'pi_po_paid' AND tenant_id = $1",
    )
    .bind(tenant_id.as_i64())
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();

    sqlx::query(
        "UPDATE payments.payments SET payout_status = 'failed' WHERE external_payment_id = 'pi_po_failed' AND tenant_id = $1",
    )
    .bind(tenant_id.as_i64())
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();

    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;

    // 1. Filtering by each of pending/paid/failed returns only matching payments.
    for (status, expected_pi) in [
        ("pending", "pi_po_pending"),
        ("paid", "pi_po_paid"),
        ("failed", "pi_po_failed"),
    ] {
        let res = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri(&format!("/api/v1/tenant/payments?payout_status={status}"))
                .to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let view: serde_json::Value = actix_test::read_body_json(res).await;
        let items = view["items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            1,
            "filtering by {status} should match exactly 1 row"
        );
        assert_eq!(items[0]["external_payment_id"], expected_pi);
        assert_eq!(items[0]["payout_status"], status);
    }

    // 2. A row with a NULL payout status is excluded when the filter is set,
    // but present when no filter is set.
    let res = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/payments")
            .to_request(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let view: serde_json::Value = actix_test::read_body_json(res).await;
    assert_eq!(
        view["items"].as_array().unwrap().len(),
        4,
        "all rows returned when unfiltered"
    );

    // 3. Unknown payout status is rejected rather than ignored.
    for bad_status in ["unknown", "paidd", "invalid"] {
        let res = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri(&format!(
                    "/api/v1/tenant/payments?payout_status={bad_status}"
                ))
                .to_request(),
        )
        .await;
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "bad payout status {bad_status} must be rejected with 400"
        );
    }

    // 4. CSV export honours the payout_status filter and rejects unknown values.
    for (status, expected_pi) in [
        ("pending", "pi_po_pending"),
        ("paid", "pi_po_paid"),
        ("failed", "pi_po_failed"),
    ] {
        let res = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri(&format!(
                    "/api/v1/tenant/payments/export?payout_status={status}"
                ))
                .to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = actix_test::read_body(res).await;
        let csv_text = std::str::from_utf8(&body).unwrap();
        let rows: Vec<&str> = csv_text
            .lines()
            .skip(1)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "CSV export with payout_status={status} must return 1 data row"
        );
        assert!(
            rows[0].contains(expected_pi),
            "CSV row must contain {expected_pi}: {}",
            rows[0]
        );
        assert!(
            !csv_text.contains("pi_po_none"),
            "NULL payout status row must not appear when payout_status={status}"
        );
    }

    let res = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/payments/export?payout_status=invalid_status")
            .to_request(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// 09.6: payouts, tax liability and the application fee.
// ---------------------------------------------------------------------------

/// A payout event envelope (`payout.created` / `payout.updated` /
/// `payout.paid` / `payout.failed`).
fn payout_event(
    event_id: &str,
    event_type: &str,
    account: &str,
    payout_id: &str,
    amount: i64,
    currency: &str,
    created: i64,
) -> serde_json::Value {
    serde_json::json!({
        "id": event_id,
        "type": event_type,
        "created": created,
        "account": account,
        "data": {"object": {
            "id": payout_id,
            "object": "payout",
            "amount": amount,
            "currency": currency,
            "status": match event_type {
                "payout.created" | "payout.updated" => "pending",
                "payout.paid" => "paid",
                _ => "failed",
            },
            "arrival_date": epoch(60),
        }}
    })
}

#[tokio::test]
async fn a_pending_payout_webhook_blocks_disconnect() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "pending-payout-disconnect").await;
    let fixture = webhook_fixture(&pool).await;
    let connection = activate_connection(&pool, tenant_id, &fixture.provider).await;
    let account = live_account_id(&fixture, tenant_id).await;

    fixture
        .deliver(&payout_event(
            "evt_po_pending",
            "payout.created",
            &account,
            "po_pending_9",
            90_000,
            "jpy",
            epoch(10),
        ))
        .await
        .unwrap();
    fixture.dispatcher.dispatch(10).await.unwrap();

    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::delete()
            .uri(&format!(
                "/api/v1/tenant/payments/connections/{}",
                connection.id
            ))
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = actix_test::read_body_json(response).await;
    assert_eq!(body["blockers"][0]["code"], "pending_payouts");
}

#[tokio::test]
async fn a_payout_paid_and_a_payout_failed_are_both_reflected_and_the_failure_alerts() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "payouts").await;
    let fixture = webhook_fixture(&pool).await;

    activate_connection(&pool, tenant_id, &fixture.provider).await;
    let account = live_account_id(&fixture, tenant_id).await;

    // The paid payout lands in the projection and fires PayoutPaid once.
    assert!(
        fixture
            .deliver(&payout_event(
                "evt_po_1",
                "payout.paid",
                &account,
                "po_paid_9",
                150_000,
                "jpy",
                epoch(10),
            ))
            .await
            .unwrap()
    );
    fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(fixture.outbox_events("payments.payout_paid").await, 1);

    // The failed payout is reflected too — and it ALERTS. The tenant's
    // money is stuck: an alert, not just a row.
    assert!(
        fixture
            .deliver(&payout_event(
                "evt_po_2",
                "payout.failed",
                &account,
                "po_failed_9",
                90_000,
                "jpy",
                epoch(20),
            ))
            .await
            .unwrap()
    );
    fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(
        fixture.outbox_events("payments.payout_failed").await,
        1,
        "a failed payout raises exactly one tenant alert"
    );

    // Both rows are webhook-truthful in the projection.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT external_payout_id, status FROM payments.payouts ORDER BY external_payout_id",
    )
    .fetch_all(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&("po_paid_9".to_string(), "paid".to_string())));
    assert!(rows.contains(&("po_failed_9".to_string(), "failed".to_string())));

    // Redelivery of either event changes nothing (dedupe by event id AND
    // by transition): still one paid event, still one alert.
    fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(fixture.outbox_events("payments.payout_paid").await, 1);
    assert_eq!(fixture.outbox_events("payments.payout_failed").await, 1);
}

/// Enable automatic tax through the use case against the fixture's fake.
async fn enable_tax(
    pool: &sqlx::PgPool,
    provider: &FakeProvider,
    tenant_id: TenantId,
) -> Result<sanvi_payments::application::TaxSettingsView, sanvi_payments::domain::PaymentsError> {
    let state = checkout_fixture(pool, provider.clone(), 1000, false);
    state.manage_tax_settings.enable(tenant_id).await
}

#[tokio::test]
async fn an_account_with_no_registrations_cannot_enable_automatic_tax_and_the_reason_is_specific() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "tax-preflight").await;
    let provider = FakeProvider::new();

    activate_connection(&pool, tenant_id, &provider).await;

    // Step 1 fails: settings active but ZERO registrations — the silent
    // under-collection trap, refused with its own specific code.
    let err = enable_tax(&pool, &provider, tenant_id).await.unwrap_err();
    match err {
        sanvi_payments::domain::PaymentsError::TaxPreflightFailed { reason } => {
            assert_eq!(reason, "no_active_registration");
        }
        other => panic!("expected the preflight refusal, got {other:?}"),
    }

    // Step 2 would fail differently: settings not active even though a
    // registration exists — each missing step names itself.
    provider.script(FakeScript {
        tax_status: Some("pending".to_string()),
        tax_registrations: Some(3),
        ..FakeScript::default()
    });
    let err = enable_tax(&pool, &provider, tenant_id).await.unwrap_err();
    match err {
        sanvi_payments::domain::PaymentsError::TaxPreflightFailed { reason } => {
            assert_eq!(reason, "tax_status_not_active");
        }
        other => panic!("expected the preflight refusal, got {other:?}"),
    }

    // Both steps hold: the toggle turns on.
    provider.script(FakeScript {
        tax_status: Some("active".to_string()),
        tax_registrations: Some(2),
        ..FakeScript::default()
    });
    let view = enable_tax(&pool, &provider, tenant_id).await.unwrap();
    assert!(view.enabled);
    assert_eq!(view.active_registrations, 2);
    assert!(view.liability_account.is_some());
}

#[tokio::test]
async fn a_session_created_with_automatic_tax_carries_the_account_liability() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "tax-liability").await;
    let provider = FakeProvider::new();

    activate_connection(&pool, tenant_id, &provider).await;

    // With tax off (the default), no session carries automatic tax.
    let _ = create_checkout_via_use_case(&pool, &provider, tenant_id, "order-tax-off").await;
    let observation = provider.last_checkout_observation().unwrap();
    assert_eq!(
        observation.automatic_tax_liability_account, None,
        "release default: no liability fields leave the process"
    );

    // Pass the preflight, flip the toggle…
    provider.script(FakeScript {
        tax_status: Some("active".to_string()),
        tax_registrations: Some(1),
        ..FakeScript::default()
    });
    enable_tax(&pool, &provider, tenant_id).await.unwrap();

    // …and the next session routes the liability to the connected
    // account — the merchant of record under direct charges — asserted
    // on what the provider actually received.
    let (_, _) = create_checkout_via_use_case(&pool, &provider, tenant_id, "order-tax-on").await;
    let observation = provider.last_checkout_observation().unwrap();
    assert_eq!(
        observation.automatic_tax_liability_account.as_deref(),
        Some(observation.external_account_id.as_str()),
        "liability.type == 'account' with the connected account id"
    );
}

#[tokio::test]
async fn lapsed_registrations_warn_on_the_scheduled_re_check() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "tax-lapse").await;
    let fixture = webhook_fixture(&pool).await;
    activate_connection(&pool, tenant_id, &fixture.provider).await;

    // Compliant at enable time…
    fixture.provider.script(FakeScript {
        tax_status: Some("active".to_string()),
        tax_registrations: Some(1),
        ..FakeScript::default()
    });
    enable_tax(&pool, &fixture.provider, tenant_id)
        .await
        .unwrap();

    // …then the registration lapses on the provider side.
    fixture.provider.script(FakeScript {
        tax_status: Some("active".to_string()),
        tax_registrations: Some(0),
        ..FakeScript::default()
    });

    // The scheduled re-check catches it and warns.
    let report = fixture.tax_recheck.execute(10).await.unwrap();
    assert_eq!(report.checked, 1, "{report:?}");
    assert_eq!(report.warned, 1, "{report:?}");
    assert_eq!(
        fixture
            .outbox_events("payments.tax_compliance_lapsed")
            .await,
        1
    );

    // The warning is stored next to the (still enabled) toggle.
    let mut scope = fixture.tx.begin_unscoped("t", "tests").await.unwrap();
    let row: (bool, Option<serde_json::Value>) =
        sqlx::query_as("SELECT enabled, warning FROM payments.tax_settings WHERE tenant_id = $1")
            .bind(tenant_id.as_i64())
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    scope.rollback().await.unwrap();
    assert!(row.0, "compliance drift never silently disables tax");
    assert!(row.1.is_some());
    assert_eq!(
        row.1.unwrap()["code"],
        "payments.tax.warning.no_active_registration"
    );
}

#[tokio::test]
async fn a_tax_event_on_the_account_triggers_the_recheck() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "tax-event").await;
    let fixture = webhook_fixture(&pool).await;
    activate_connection(&pool, tenant_id, &fixture.provider).await;

    fixture.provider.script(FakeScript {
        tax_status: Some("active".to_string()),
        tax_registrations: Some(1),
        ..FakeScript::default()
    });
    enable_tax(&pool, &fixture.provider, tenant_id)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .outbox_events("payments.tax_compliance_lapsed")
            .await,
        0
    );

    // The registration lapses; Stripe tells us with tax.settings.updated.
    fixture.provider.script(FakeScript {
        tax_status: Some("active".to_string()),
        tax_registrations: Some(0),
        ..FakeScript::default()
    });
    let event = serde_json::json!({
        "id": "evt_tax_1",
        "type": "tax.settings.updated",
        "created": epoch(40),
        "account": live_account_id(&fixture, tenant_id).await,
        "data": {"object": {"object": "tax.settings"}}
    });
    assert!(fixture.deliver(&event).await.unwrap());
    fixture.dispatcher.dispatch(10).await.unwrap();

    assert_eq!(
        fixture
            .outbox_events("payments.tax_compliance_lapsed")
            .await,
        1,
        "the webhook-triggered recheck warned about the lapse"
    );
}

#[tokio::test]
async fn the_platform_fee_is_disclosed_on_the_payment_when_charged() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "fee-disclosure").await;
    let fixture = webhook_fixture(&pool).await;
    activate_connection(&pool, tenant_id, &fixture.provider).await;
    let account = live_account_id(&fixture, tenant_id).await;

    // A checkout whose PaymentIntent carried a platform fee (fees are ON
    // here only for this test — the release default stays off everywhere).
    let (checkout_id, _session_id) =
        create_checkout_via_use_case(&pool, &fixture.provider, tenant_id, "order-fee").await;

    // The provider confirms the payment WITH the application fee it took:
    // 5% of ¥2000 = ¥100 to the platform, ¥1900 remaining the tenant's.
    let object = serde_json::json!({
        "id": "pi_fee_1",
        "object": "payment_intent",
        "amount": 2000,
        "currency": "jpy",
        "application_fee_amount": 100,
        "metadata": {
            "checkout_id": checkout_id.to_string(),
            "tenant_id": tenant_id.to_string()
        }
    });
    let event = serde_json::json!({
        "id": "evt_fee_1",
        "type": "payment_intent.succeeded",
        "created": epoch(50),
        "account": account,
        "data": {"object": object}
    });
    assert!(fixture.deliver(&event).await.unwrap());
    fixture.dispatcher.dispatch(10).await.unwrap();

    // The fee is disclosed on the payment read model the tenant sees.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let row: (i64, String, i64, String) = sqlx::query_as(
        "SELECT amount_minor, currency,
                COALESCE(fee_amount_minor, -1), COALESCE(fee_currency, '')
         FROM payments.payments WHERE external_payment_id = 'pi_fee_1'",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(row.0, 2000, "the total was never altered by rounding");
    assert_eq!(row.1, "JPY");
    assert_eq!(row.2, 100, "the platform's fee appears on the charge");
    assert_eq!(row.3, "JPY");
    assert_eq!(2000 - row.2, 1900, "the remainder is the tenant's");

    // And exactly one payment-succeeded event fired for this order.
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE event_type = 'payments.payment_succeeded'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
}

// ---------------------------------------------------------------------------
// 09.7: disconnect — guards, local semantics, reconnect, and the promise
// that money truth never stops being recorded.
// ---------------------------------------------------------------------------

/// Seed one payment row against `connection_id` through its checkout.
async fn seed_payment(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    connection_id: ConnectionId,
    external_payment_id: &str,
    status: &str,
) -> (CheckoutId, PaymentId) {
    let checkout_id =
        CheckoutId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id"));
    let payment_id =
        PaymentId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id"));
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query(
        "INSERT INTO payments.checkouts (id, tenant_id, connection_id, reference, amount_minor, currency, status)
         VALUES ($1, $2, $3, 'order-seed', 2000, 'JPY', 'open')",
    )
    .bind(checkout_id.as_i64())
    .bind(tenant_id.as_i64())
    .bind(connection_id.as_i64())
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO payments.payments (id, tenant_id, checkout_id, external_payment_id, status, amount_minor, currency)
         VALUES ($1, $2, $3, $4, $5, 2000, 'JPY')",
    )
    .bind(payment_id.as_i64())
    .bind(tenant_id.as_i64())
    .bind(checkout_id.as_i64())
    .bind(external_payment_id)
    .bind(status)
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();
    (checkout_id, payment_id)
}

/// Seed the state produced after checkout creation but before any payment
/// webhook has arrived.
async fn seed_open_checkout(pool: &sqlx::PgPool, tenant_id: TenantId, connection_id: ConnectionId) {
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query(
        "INSERT INTO payments.checkouts (id, tenant_id, connection_id, reference, amount_minor, currency, status)
         VALUES ($1, $2, $3, 'order-open', 2000, 'JPY', 'open')",
    )
    .bind(CheckoutId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")).as_i64())
    .bind(tenant_id.as_i64())
    .bind(connection_id.as_i64())
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();
}

#[tokio::test]
async fn disconnect_refuses_each_blocker_and_names_it_in_the_response() {
    /// Which money guard to exercise.
    enum Seed {
        InFlight,
        Dispute,
        Payout,
    }

    // Each guard is exercised in its own database: in-flight payments,
    // open disputes and pending payouts each refuse with their own
    // stable code.
    async fn run_case(label: &'static str, seed: Seed, code: &str) {
        let db = sanvi_testsupport::test_db().await;
        let pool = db.app_pool();
        let tenant_id = seed_tenant(&pool, "blocker").await;
        let provider = FakeProvider::new();
        let connection = activate_connection(&pool, tenant_id, &provider).await;

        match seed {
            Seed::InFlight => {
                seed_open_checkout(&pool, tenant_id, connection.id).await;
            }
            Seed::Dispute => {
                let (_, payment_id) =
                    seed_payment(&pool, tenant_id, connection.id, "pi_disputed", "succeeded").await;
                let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
                let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
                sqlx::query(
                    "INSERT INTO payments.disputes
                       (id, tenant_id, payment_id, external_dispute_id, status,
                        amount_minor, currency, created_at, updated_at)
                     VALUES ($1, $2, $3, 'dp_test_1', 'needs_response', 2000, 'JPY', now(), now())",
                )
                .bind(
                    DisputeId::from_snowflake(
                        sanvi_testsupport::test_ids().next_id().expect("test id"),
                    )
                    .as_i64(),
                )
                .bind(tenant_id.as_i64())
                .bind(payment_id.as_i64())
                .execute(&mut **scope.tx_mut().as_inner())
                .await
                .unwrap();
                scope.commit().await.unwrap();
            }
            Seed::Payout => {
                let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
                let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
                sqlx::query(
                    "INSERT INTO payments.payouts
                       (id, tenant_id, connection_id, external_payout_id, amount_minor, currency, status)
                     VALUES ($1, $2, $3, 'po_test_1', 1500, 'JPY', 'pending')",
                )
                .bind(PayoutId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")).as_i64())
                .bind(tenant_id.as_i64())
                .bind(connection.id.as_i64())
                .execute(&mut **scope.tx_mut().as_inner())
                .await
                .unwrap();
                scope.commit().await.unwrap();
            }
        }

        let state = checkout_fixture(&pool, provider.clone(), 1000, false);
        let app = checkout_app(state.clone(), tenant_id).await;
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::delete()
                .uri(&format!(
                    "/api/v1/tenant/payments/connections/{}",
                    connection.id
                ))
                .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{label}");
        let body: serde_json::Value = actix_test::read_body_json(response).await;
        assert!(
            body["type"]
                .as_str()
                .unwrap()
                .ends_with("payments/disconnect-blocked"),
            "{label}: {body}"
        );
        assert_eq!(body["blockers"][0]["code"], code, "{label}: {body}");
        assert_eq!(
            body["blockers"][0]["summary_key"],
            format!("payments.blocker.{code}"),
            "{label}"
        );
    }

    run_case("in-flight payment", Seed::InFlight, "in_flight_payments").await;
    run_case("open dispute", Seed::Dispute, "open_disputes").await;
    run_case("pending payout", Seed::Payout, "pending_payouts").await;
}

#[tokio::test]
async fn privacy_erasure_scrubs_customer_ref_but_retains_transaction_record() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "payments-privacy").await;
    let provider = FakeProvider::new();
    let connection = activate_connection(&pool, tenant_id, &provider).await;
    let (checkout_id, _) =
        seed_payment(&pool, tenant_id, connection.id, "pi_privacy", "succeeded").await;

    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query(
        "UPDATE payments.checkouts
         SET customer_ref = '{\"buyer\": {\"id\": \"payer-123\"}}'::jsonb
         WHERE id = $1",
    )
    .bind(checkout_id.as_i64())
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();

    let privacy = sanvi_payments::adapters::PaymentsPrivacyProvider::new(TxManager::new(
        pool.clone(),
        sanvi_testsupport::test_ids(),
    ));
    let subject = SubjectRef::end_user(tenant_id, "payer-123");
    let bundle = privacy.collect(&subject).await.unwrap();
    assert_eq!(bundle.entries.len(), 1);
    assert_eq!(bundle.entries[0].data_class, DataClass::TransactionRecord);
    assert_eq!(
        bundle.entries[0].value["checkout_id"],
        serde_json::json!(checkout_id.as_i64())
    );

    let report = privacy.erase(&subject, EraseMode::Full).await.unwrap();
    assert_eq!(report.anonymised, 1);
    assert!(
        report
            .retained
            .iter()
            .any(|item| item.data_class == DataClass::TransactionRecord)
    );

    let verify_tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = verify_tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let customer_ref: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT customer_ref FROM payments.checkouts WHERE id = $1")
            .bind(checkout_id.as_i64())
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    scope.rollback().await.unwrap();
    assert!(customer_ref.is_none(), "customer reference must be erased");
}

#[tokio::test]
async fn disconnect_when_clean_stops_checkout_keeps_history_and_allows_reconnect() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "clean-disconnect").await;
    let provider = FakeProvider::new();
    let connection = activate_connection(&pool, tenant_id, &provider).await;

    // History: one settled payment on the soon-to-be-old connection.
    let (_, payment_id) =
        seed_payment(&pool, tenant_id, connection.id, "pi_hist", "succeeded").await;

    let state = checkout_fixture(&pool, provider.clone(), 1000, false);
    let app = checkout_app(state.clone(), tenant_id).await;

    // Clean disconnect succeeds and reports the new status.
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::delete()
            .uri(&format!(
                "/api/v1/tenant/payments/connections/{}",
                connection.id
            ))
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = actix_test::read_body_json(response).await;
    assert_eq!(body["status"], "disconnected");

    // Checkout immediately refuses afterwards.
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(checkout_body("order-after", 500, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let err_body: serde_json::Value = actix_test::read_body_json(response).await;
    assert!(
        err_body["type"]
            .as_str()
            .unwrap()
            .ends_with("payments/connection-not-active"),
        "{err_body}"
    );

    // Historical records stay queryable (GET /payments).
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/payments")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let list: serde_json::Value = actix_test::read_body_json(response).await;
    let ids: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&payment_id.to_string().as_str()),
        "old payments stay queryable after disconnect"
    );

    // Reconnect creates a NEW connection row; the old id stays history.
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/payments/connections")
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(serde_json::json!({ "provider": "fake" }))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let reconnected: serde_json::Value = actix_test::read_body_json(response).await;
    assert_ne!(
        reconnected["id"].as_str().unwrap(),
        connection.id.to_string()
    );
    assert_eq!(reconnected["status"], "pending");

    // The disconnect was audited (audit entries are tenant-RLS'd: read
    // through a scoped transaction, exactly like the shared policy's own
    // tenant admission).
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit.entries WHERE action = 'payments.connection_disconnected'",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(audited, 1);
}

#[tokio::test]
async fn deleting_an_already_disconnected_connection_is_an_idempotent_ok() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "idem-delete").await;
    let provider = FakeProvider::new();
    let connection = activate_connection(&pool, tenant_id, &provider).await;

    let state = checkout_fixture(&pool, provider.clone(), 1000, false);
    let app = checkout_app(state.clone(), tenant_id).await;
    let uri = format!("/api/v1/tenant/payments/connections/{}", connection.id);
    for _ in 0..2 {
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::delete()
                .uri(&uri)
                .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    // Exactly one disconnect event despite two DELETEs.
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE event_type = 'payments.provider_disconnected'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
}

#[tokio::test]
async fn a_payment_completing_after_disconnect_is_still_recorded() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let fixture = webhook_fixture(&pool).await;
    let tenant_id = seed_tenant(&pool, "post-disconnect-money").await;

    // A connection that existed and then was disconnected.
    let provider = FakeProvider::new();
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(provider.clone())).unwrap();
    let repo = SqlxProviderConnectionRepository::new();
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
    let mut connection = ProviderConnection::pending(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        ProviderKind::Fake,
        account.external_account_id.clone(),
        Utc::now(),
    )
    .unwrap();
    connection.disconnect(Utc::now()).unwrap();
    {
        let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
        repo.insert(&mut scope, &connection).await.unwrap();
        scope.commit().await.unwrap();
    }

    // The customer pays a session minted before the disconnect.
    let event = payment_intent_event(
        "evt_after_disconnect",
        "payment_intent.succeeded",
        &account.external_account_id,
        CheckoutId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        "pi_after_disconnect",
        None,
        epoch(0),
    );
    fixture.deliver(&event).await.unwrap();
    fixture.dispatcher.dispatch(10).await.unwrap();

    // If the handler refused the event, say so before the money asserts.
    let error: Option<String> = sqlx::query_scalar(
        "SELECT COALESCE(error, '') FROM payments.webhook_events
         WHERE stripe_event_id = 'evt_after_disconnect'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(error.as_deref(), Some(""), "handler error: {error:?}");

    // The money has a local record even though the connection is gone.
    // (Reads go through a tenant-scoped transaction: RLS hides rows from
    // a bare pooled connection.)
    let mut scope = fixture.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let recorded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM payments.payments WHERE external_payment_id = 'pi_after_disconnect'",
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.rollback().await.unwrap();
    assert_eq!(recorded, 1, "money never moves without a local record");
    let events: i64 = fixture.outbox_events("payments.payment_succeeded").await;
    assert_eq!(events, 1);
}

#[tokio::test]
async fn the_success_url_carries_the_checkout_id_back_to_the_confirmation_page() {
    // Review finding: the buyer returned to a page whose only clue to
    // which checkout it was confirming lived in tab-scoped session
    // storage — so a return into a different tab, or with storage
    // unavailable, showed "not found" over a payment that succeeded.
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "success-url-id").await;
    let provider = FakeProvider::new();
    activate_connection(&pool, tenant_id, &provider).await;

    let state = checkout_fixture(&pool, provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;

    let created = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/v1/tenant/checkout")
            .insert_header(("Idempotency-Key", Uuid::now_v7().to_string()))
            .set_json(checkout_body("order-success-url", 2000, "JPY"))
            .to_request(),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let body: serde_json::Value = actix_test::read_body_json(created).await;
    let checkout_id = body["id"].as_str().expect("checkout id").to_string();

    let observed = provider
        .last_checkout_observation()
        .expect("the provider saw a session")
        .success_url;
    assert_eq!(
        observed,
        format!("https://shop.test/thanks?checkout_id={checkout_id}"),
        "the id the confirmation page needs rides on the redirect"
    );
    assert!(
        observed.starts_with("https://shop.test/"),
        "the authority the allow-list vetted is untouched"
    );
}

#[tokio::test]
async fn a_declined_card_surfaces_its_code_so_the_storefront_can_say_why() {
    // Review finding: the storefront mapped decline codes to specific
    // copy, but nothing ever gave it one — every decline rendered the
    // generic "something went wrong".
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "decline-code").await;
    let fixture = webhook_fixture(&pool).await;
    let connection = activate_connection(&pool, tenant_id, &fixture.provider).await;
    let account = connection.external_account_id.clone();

    let (checkout_id, _session_id) =
        create_checkout_via_use_case(&pool, &fixture.provider, tenant_id, "order-declined").await;

    assert!(
        fixture
            .deliver(&payment_intent_event(
                "evt_declined_view",
                "payment_intent.payment_failed",
                &account,
                checkout_id,
                tenant_id,
                "pi_declined_view",
                Some("insufficient_funds"),
                epoch(7),
            ))
            .await
            .unwrap()
    );
    let report = fixture.dispatcher.dispatch(10).await.unwrap();
    assert_eq!(report.processed, 1, "{report:?}");

    let state = checkout_fixture(&pool, fixture.provider.clone(), 1000, false);
    let app = checkout_app(state, tenant_id).await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri(&format!("/api/v1/tenant/checkout/{checkout_id}"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let view: serde_json::Value = actix_test::read_body_json(response).await;
    assert_eq!(view["status"], "failed");
    assert_eq!(
        view["failure_code"], "insufficient_funds",
        "the code the buyer's message is built from"
    );
    assert!(
        view.get("failure_message").is_none(),
        "the provider's own wording never reaches a buyer"
    );
}

#[tokio::test]
async fn the_checkout_config_answers_readiness_before_a_buyer_clicks_pay() {
    // Review finding: the Pay button was always enabled, so a tenant that
    // cannot be paid was discovered only by clicking and eating a 409.
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let provider = FakeProvider::new();

    // No connection at all: onboarding never started.
    let untouched = seed_tenant(&pool, "config-none").await;
    let app = checkout_app(
        checkout_fixture(&pool, provider.clone(), 1000, false),
        untouched,
    )
    .await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/checkout/config")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let view: serde_json::Value = actix_test::read_body_json(response).await;
    assert_eq!(view["can_accept_payments"], false);
    assert_eq!(
        view["cannot_accept_reason"], "payments.blocker.onboarding_not_started",
        "an i18n key, never a rendered sentence"
    );
    assert_eq!(view["currency"], serde_json::Value::Null);

    // Onboarding started but the capability is not active yet.
    let pending = seed_tenant(&pool, "config-pending").await;
    pending_connection(&pool, pending, &provider).await;
    let app = checkout_app(
        checkout_fixture(&pool, provider.clone(), 1000, false),
        pending,
    )
    .await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/checkout/config")
            .to_request(),
    )
    .await;
    let view: serde_json::Value = actix_test::read_body_json(response).await;
    assert_eq!(view["can_accept_payments"], false);
    assert_eq!(
        view["cannot_accept_reason"],
        "payments.blocker.onboarding_not_started"
    );

    // Active: the storefront may price, in the account's own currency.
    let ready = seed_tenant(&pool, "config-ready").await;
    let connection = activate_connection(&pool, ready, &provider).await;
    let app = checkout_app(
        checkout_fixture(&pool, provider.clone(), 1000, false),
        ready,
    )
    .await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/checkout/config")
            .to_request(),
    )
    .await;
    let view: serde_json::Value = actix_test::read_body_json(response).await;
    assert_eq!(view["can_accept_payments"], true);
    assert_eq!(view["cannot_accept_reason"], serde_json::Value::Null);
    assert_eq!(
        view["currency"],
        serde_json::json!(connection.default_currency),
        "the currency the buy path pre-flights against"
    );
    assert_eq!(
        view["tax_enabled"], false,
        "an active connection with no tax settings collects no tax"
    );

    // Enabling automatic tax flips the flag the storefront's tax note is
    // drawn from, so the buyer is told what the session will actually do.
    provider.script(FakeScript {
        tax_status: Some("active".to_string()),
        tax_registrations: Some(2),
        ..FakeScript::default()
    });
    enable_tax(&pool, &provider, ready).await.unwrap();
    let app = checkout_app(
        checkout_fixture(&pool, provider.clone(), 1000, false),
        ready,
    )
    .await;
    let response = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/tenant/checkout/config")
            .to_request(),
    )
    .await;
    let view: serde_json::Value = actix_test::read_body_json(response).await;
    assert_eq!(view["tax_enabled"], true);
}
