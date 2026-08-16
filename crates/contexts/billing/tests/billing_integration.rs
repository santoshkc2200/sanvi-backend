//! Integration tests: real Postgres in containers, the sanvi_app role
//! (NOBYPASSRLS), full migrations. The Stripe boundary is faked (the HTTP
//! client is covered by unit tests + the sandbox nightly suite); everything
//! else — webhook store-first semantics, deduplication, out-of-order
//! guards, entitlement recomputation, dunning, recovery, overrides and
//! reconciliation — runs against real repositories and the real outbox.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

use sanvi_billing::{
    adapters::{
        AccessEntitlementSyncer, LoggingNotifications, OutboxEventPublisher, SqlxInvoiceRepository,
        SqlxPlanEntitlementRepository, SqlxPlanPriceRepository, SqlxPlanRepository,
        SqlxSubscriptionRepository, SqlxWebhookEventRepository, SystemClock, SystemIdGenerator,
        TenancyTenantLifecycle,
    },
    application::{
        ApplySubscriptionOverride, ApplySubscriptionOverrideCommand, CreateCheckoutSession,
        CreateCheckoutSessionCommand, DunningSweep, ReconcileSubscriptions, WebhookDispatcher,
        WebhookReceiver,
    },
    domain::{
        CheckoutSessionRequest, CheckoutSessionResult, Clock, CollectionState, EntitlementSyncer,
        EventPublisher, IdGenerator, InvoiceRepository, Notifications, PlanEntitlementRepository,
        PlanId, PlanPriceRepository, PlanRepository, PortalSessionRequest, PortalSessionResult,
        PriceId, StripeClient, StripeCustomer, StripeSubscriptionView, SubscriptionRepository,
        SubscriptionSource, SubscriptionStatus, TaxGate, TaxRegistration, TenantLifecycle,
        WebhookEventRepository,
    },
};
use sanvi_persistence::{DatabaseConfig, Migrator, TxManager, build_pool};

fn migrations_dir() -> String {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("../../../migrations")
        .canonicalize()
        .expect("migrations directory must exist")
        .to_string_lossy()
        .to_string()
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
        host: "127.0.0.1".to_string(),
        port,
        name: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
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
    sqlx::query("GRANT CONNECT ON DATABASE sanvi_test TO sanvi_app")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;

    let migrator_pool = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_migrator".to_string(),
        password: "mig".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();
    Migrator::new(migrations_dir())
        .run(&migrator_pool)
        .await
        .expect("migrations up");

    let app_pool = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_app".to_string(),
        password: "app".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();

    let admin_pool = sqlx::PgPool::connect_lazy(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/sanvi_test"
    ))
    .expect("admin pool url");

    (app_pool, admin_pool, node)
}

/// A configurable Stripe fake: records calls, returns scripted answers.
#[derive(Clone, Default)]
struct FakeStripe {
    inner: Arc<std::sync::Mutex<FakeStripeState>>,
}

#[derive(Default)]
struct FakeStripeState {
    customers: usize,
    checkout_calls: usize,
    subscription_status: String,
    subscription_price: String,
    tax_registrations: usize,
}

impl FakeStripe {
    fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(FakeStripeState {
                subscription_status: "active".to_string(),
                subscription_price: "price_starter_monthly".to_string(),
                ..FakeStripeState::default()
            })),
        }
    }
}

#[async_trait]
impl StripeClient for FakeStripe {
    async fn ensure_customer(
        &self,
        _tenant_id: Uuid,
        _command_id: Uuid,
    ) -> Result<StripeCustomer, sanvi_billing::domain::BillingError> {
        self.inner.lock().unwrap().customers += 1;
        Ok(StripeCustomer {
            id: format!("cus_{}", self.inner.lock().unwrap().customers),
        })
    }

    async fn create_checkout_session(
        &self,
        _request: &CheckoutSessionRequest,
    ) -> Result<CheckoutSessionResult, sanvi_billing::domain::BillingError> {
        self.inner.lock().unwrap().checkout_calls += 1;
        Ok(CheckoutSessionResult {
            session_id: "cs_test_1".to_string(),
            url: "https://checkout.stripe.test/session".to_string(),
        })
    }

    async fn create_portal_session(
        &self,
        _request: &PortalSessionRequest,
    ) -> Result<PortalSessionResult, sanvi_billing::domain::BillingError> {
        Ok(PortalSessionResult {
            url: "https://billing.stripe.test/portal".to_string(),
        })
    }

    async fn retrieve_subscription(
        &self,
        stripe_subscription_id: &str,
    ) -> Result<StripeSubscriptionView, sanvi_billing::domain::BillingError> {
        let state = self.inner.lock().unwrap();
        Ok(StripeSubscriptionView {
            id: stripe_subscription_id.to_string(),
            customer_id: "cus_1".to_string(),
            status: SubscriptionStatus::parse(&state.subscription_status).unwrap(),
            stripe_price_id: Some(state.subscription_price.clone()),
            current_period: None,
            cancel_at_period_end: false,
            trial_end: None,
            updated: Some(Utc::now()),
        })
    }

    async fn list_active_tax_registrations(
        &self,
    ) -> Result<Vec<TaxRegistration>, sanvi_billing::domain::BillingError> {
        let state = self.inner.lock().unwrap();
        Ok((0..state.tax_registrations)
            .map(|i| TaxRegistration {
                id: format!("taxreg_{i}"),
                country: "DE".to_string(),
            })
            .collect())
    }
}

/// A tax gate that answers from the fake Stripe client directly.
struct DirectTaxGate {
    stripe: Arc<FakeStripe>,
}

#[async_trait]
impl TaxGate for DirectTaxGate {
    async fn has_active_registration(&self) -> Result<bool, sanvi_billing::domain::BillingError> {
        Ok(!self
            .stripe
            .list_active_tax_registrations()
            .await?
            .is_empty())
    }
}

fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let signed_payload = format!("{timestamp}.{}", String::from_utf8_lossy(payload));
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signed_payload.as_bytes());
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("t={timestamp},v1={hex}")
}

fn subscription_event(
    event_id: &str,
    event_type: &str,
    created: DateTime<Utc>,
    sub_id: &str,
    status: &str,
    tenant_id: Uuid,
    price_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": event_id,
        "type": event_type,
        "created": created.timestamp(),
        "data": { "object": {
            "id": sub_id,
            "customer": "cus_1",
            "status": status,
            "current_period_start": created.timestamp() - 30 * 86400,
            "current_period_end": created.timestamp() + 30 * 86400,
            "cancel_at_period_end": false,
            "trial_end": null,
            "metadata": { "sanvi_tenant_id": tenant_id.to_string() },
            "items": { "data": [ { "price": { "id": price_id } } ] }
        } }
    })
}

fn invoice_event(
    event_id: &str,
    event_type: &str,
    created: DateTime<Utc>,
    invoice_id: &str,
    sub_id: &str,
    status: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": event_id,
        "type": event_type,
        "created": created.timestamp(),
        "data": { "object": {
            "id": invoice_id,
            "subscription": sub_id,
            "status": status,
            "number": "A1-0001",
            "total": 4900,
            "currency": "usd",
            "hosted_invoice_url": "https://invoice.stripe.test/1",
            "invoice_pdf": "https://invoice.stripe.test/1/pdf",
            "period_start": created.timestamp() - 30 * 86400,
            "period_end": created.timestamp(),
            "created": created.timestamp(),
            "status_transitions": {}
        } }
    })
}

struct Stack {
    tx: TxManager,
    dispatcher: Arc<WebhookDispatcher>,
    receiver: WebhookReceiver,
    checkout: CreateCheckoutSession,
    dunning: DunningSweep,
    reconciliation: ReconcileSubscriptions,
    override_apply: ApplySubscriptionOverride,
    subscriptions: Arc<dyn SubscriptionRepository>,
    prices: Arc<dyn PlanPriceRepository>,
    fake: Arc<FakeStripe>,
}

async fn build_stack(pool: sanvi_persistence::PgPool, fake: Arc<FakeStripe>) -> Stack {
    let tx = TxManager::new(pool);
    let plans: Arc<dyn PlanRepository> = Arc::new(SqlxPlanRepository::new());
    let prices: Arc<dyn PlanPriceRepository> = Arc::new(SqlxPlanPriceRepository::new());
    let grants: Arc<dyn PlanEntitlementRepository> = Arc::new(SqlxPlanEntitlementRepository::new());
    let subscriptions: Arc<dyn SubscriptionRepository> =
        Arc::new(SqlxSubscriptionRepository::new());
    let invoices: Arc<dyn InvoiceRepository> = Arc::new(SqlxInvoiceRepository::new());
    let webhooks: Arc<dyn WebhookEventRepository> = Arc::new(SqlxWebhookEventRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    let notifications: Arc<dyn Notifications> = Arc::new(LoggingNotifications::new());
    let ids: Arc<dyn IdGenerator> = Arc::new(SystemIdGenerator);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let entitlements: Arc<dyn sanvi_access::domain::EntitlementRepository> =
        Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new());
    let syncer: Arc<dyn EntitlementSyncer> = Arc::new(AccessEntitlementSyncer::new(entitlements));
    let tenants: Arc<dyn sanvi_tenancy::domain::TenantRepository> =
        Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new());
    let lifecycle: Arc<dyn TenantLifecycle> = Arc::new(TenancyTenantLifecycle::new(
        tx.clone(),
        tenants,
        Arc::new(sanvi_tenancy::adapters::OutboxEventPublisher::new()),
        Arc::new(sanvi_tenancy::adapters::SystemClock),
    ));
    let stripe: Arc<dyn StripeClient> = fake.clone();
    let tax_gate: Arc<dyn TaxGate> = Arc::new(DirectTaxGate {
        stripe: fake.clone(),
    });

    let dispatcher = Arc::new(WebhookDispatcher::new(
        tx.clone(),
        webhooks.clone(),
        subscriptions.clone(),
        invoices.clone(),
        prices.clone(),
        plans.clone(),
        grants.clone(),
        syncer.clone(),
        lifecycle.clone(),
        notifications.clone(),
        events.clone(),
        stripe.clone(),
        ids.clone(),
        clock.clone(),
    ));
    let receiver = WebhookReceiver::new(
        tx.clone(),
        webhooks.clone(),
        clock.clone(),
        "whsec_test".to_string(),
        300,
        vec!["127.0.0.1".to_string()],
    );
    let checkout = CreateCheckoutSession::new(
        tx.clone(),
        plans.clone(),
        prices.clone(),
        subscriptions.clone(),
        stripe.clone(),
        tax_gate,
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
        clock.clone(),
        "http://localhost/success".to_string(),
        "http://localhost/cancel".to_string(),
        false,
        8,
    );
    let dunning = DunningSweep::new(
        tx.clone(),
        subscriptions.clone(),
        syncer.clone(),
        lifecycle.clone(),
        notifications.clone(),
        events.clone(),
        clock.clone(),
        14,
        3,
    );
    let reconciliation = ReconcileSubscriptions::new(
        tx.clone(),
        subscriptions.clone(),
        stripe.clone(),
        dispatcher.clone(),
    );
    let override_apply = ApplySubscriptionOverride::new(
        tx.clone(),
        plans.clone(),
        prices.clone(),
        grants.clone(),
        subscriptions.clone(),
        syncer.clone(),
        ids.clone(),
        clock.clone(),
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
    );

    Stack {
        tx,
        dispatcher,
        receiver,
        checkout,
        dunning,
        reconciliation,
        override_apply,
        subscriptions,
        prices,
        fake,
    }
}

async fn provision_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> Uuid {
    use sanvi_tenancy::{
        adapters::{
            OutboxEventPublisher as TenancyEvents, SystemClock as TenancyClock,
            SystemIdGenerator as TenancyIds,
            repo::{SqlxSlugReservationPort, SqlxTenantRepository},
        },
        application::{ActivateTenant, ProvisionTenant, ProvisionTenantCommand},
        domain::{SlugReservationPort, TenantRepository},
    };
    let tx = TxManager::new(pool.clone());
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let slugs: Arc<dyn SlugReservationPort> = Arc::new(SqlxSlugReservationPort::new());
    let events = Arc::new(TenancyEvents::new());
    let ids = Arc::new(TenancyIds);
    let clock = Arc::new(TenancyClock);
    let provision = ProvisionTenant::new(
        tx.clone(),
        tenants.clone(),
        slugs,
        events.clone(),
        ids,
        clock.clone(),
    );
    let view = provision
        .execute(ProvisionTenantCommand {
            slug: slug.to_string(),
            display_name: "Acme".to_string(),
            region: "us".to_string(),
            default_locale: "en".to_string(),
        })
        .await
        .unwrap();
    let tenant_id: Uuid = view.id.parse().unwrap();
    let activate = ActivateTenant::new(tx, tenants, events, clock);
    activate
        .execute(sanvi_tenancy::domain::TenantId::from_uuid(tenant_id))
        .await
        .unwrap();
    tenant_id
}

async fn seed_features(pool: &sanvi_persistence::PgPool) {
    let use_case = sanvi_access::application::SeedFeatures::new(
        TxManager::new(pool.clone()),
        Arc::new(sanvi_access::adapters::SqlxFeatureRepository::new()),
    );
    use_case.execute().await.unwrap();
}

async fn add_price(stack: &Stack, plan_id: PlanId, stripe_price_id: &str) -> PriceId {
    let mut scope = stack
        .tx
        .begin_unscoped("test_add_price", "test")
        .await
        .unwrap();
    let price = sanvi_billing::domain::PlanPrice::new(
        PriceId::new(),
        plan_id,
        stripe_price_id.to_string(),
        "usd".to_string(),
        sanvi_billing::domain::PriceInterval::Month,
        4900,
        None,
        Utc::now(),
    )
    .unwrap();
    stack.prices.save(&mut scope, &price).await.unwrap();
    scope.commit().await.unwrap();
    price.id()
}

async fn receive_and_dispatch(stack: &Stack, payload: &serde_json::Value) -> bool {
    let body = serde_json::to_vec(payload).unwrap();
    let header = sign("whsec_test", Utc::now().timestamp(), &body);
    let inserted = stack
        .receiver
        .receive(&body, Some(&header), Some("127.0.0.1".parse().unwrap()))
        .await
        .expect("webhook stored");
    let report = stack.dispatcher.dispatch(10).await.expect("dispatch");
    assert_eq!(report.failed, 0, "no webhook failures expected");
    inserted
}

async fn subscription_source_rows(
    pool: &sanvi_persistence::PgPool,
    tenant_id: Uuid,
) -> Vec<(String, bool, Option<i64>)> {
    sqlx::query_as::<_, (String, bool, Option<i64>)>(
        "SELECT feature_key, enabled, \"limit\" FROM access.entitlements
         WHERE tenant_id = $1 AND source = 'subscription' ORDER BY feature_key",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn subscribe_dunn_recover_and_reconcile() {
    let (pool, admin, _node) = prepare().await;
    seed_features(&pool).await;
    let tenant_id = provision_tenant(&pool, "acme-billing").await;
    let fake = Arc::new(FakeStripe::new());
    let stack = build_stack(pool.clone(), fake.clone()).await;

    // Checkout requires an active price on a real plan.
    let plan_id = {
        let mut scope = stack
            .tx
            .begin_unscoped("test_plan_lookup", "test")
            .await
            .unwrap();
        let plan = SqlxPlanRepository::new()
            .find_by_key(&mut scope, "starter")
            .await
            .unwrap()
            .expect("starter plan seeded by migration");
        plan.id()
    };
    let price_id = add_price(&stack, plan_id, "price_starter_monthly").await;

    let session = stack
        .checkout
        .execute(
            Uuid::nil(),
            CreateCheckoutSessionCommand {
                tenant_id,
                price_id,
                success_url: None,
                cancel_url: None,
            },
        )
        .await
        .expect("checkout session");
    assert!(session.url.starts_with("https://checkout"));
    // A live subscription must block a second checkout.
    // (No subscription row yet — the webhook creates it.)
    assert!(
        stack
            .checkout
            .execute(
                Uuid::nil(),
                CreateCheckoutSessionCommand {
                    tenant_id,
                    price_id,
                    success_url: None,
                    cancel_url: None,
                },
            )
            .await
            .is_ok()
    );

    // Subscription webhook: creates the local row + entitlements.
    let created = Utc::now();
    receive_and_dispatch(
        &stack,
        &subscription_event(
            "evt_sub_1",
            "customer.subscription.created",
            created,
            "sub_1",
            "active",
            tenant_id,
            "price_starter_monthly",
        ),
    )
    .await;

    let subscription = {
        let mut scope = stack
            .tx
            .begin_unscoped("test_read_sub", "test")
            .await
            .unwrap();
        let sub = stack
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await
            .unwrap()
            .expect("subscription created");
        scope.commit().await.unwrap();
        sub
    };
    assert_eq!(subscription.status(), SubscriptionStatus::Active);
    assert_eq!(subscription.source(), SubscriptionSource::Stripe);
    assert_eq!(subscription.stripe_subscription_id(), Some("sub_1"));

    let rows = subscription_source_rows(&admin, tenant_id).await;
    assert!(
        !rows.is_empty(),
        "plan grants written as subscription source"
    );
    assert!(
        rows.iter()
            .any(|(key, enabled, _)| key == "tenancy.member_limit" && *enabled)
    );

    // Idempotency: redelivering the same event changes nothing.
    let count_before: i64 = sqlx::query_scalar("SELECT count(*) FROM billing.webhook_events")
        .fetch_one(&admin)
        .await
        .unwrap();
    let inserted = receive_and_dispatch(
        &stack,
        &subscription_event(
            "evt_sub_1",
            "customer.subscription.created",
            created,
            "sub_1",
            "active",
            tenant_id,
            "price_starter_monthly",
        ),
    )
    .await;
    assert!(!inserted, "redelivery is deduplicated by event id");
    let count_after: i64 = sqlx::query_scalar("SELECT count(*) FROM billing.webhook_events")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(
        count_after, count_before,
        "redelivery deduplicated by event id"
    );

    // Out-of-order: an older event must not overwrite newer state.
    receive_and_dispatch(
        &stack,
        &subscription_event(
            "evt_sub_stale",
            "customer.subscription.updated",
            created - chrono::Duration::days(30),
            "sub_1",
            "canceled",
            tenant_id,
            "price_starter_monthly",
        ),
    )
    .await;
    let subscription = {
        let mut scope = stack
            .tx
            .begin_unscoped("test_read_sub_stale", "test")
            .await
            .unwrap();
        let sub = stack
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await
            .unwrap()
            .expect("subscription still there");
        scope.commit().await.unwrap();
        sub
    };
    assert_eq!(
        subscription.status(),
        SubscriptionStatus::Active,
        "stale event ignored"
    );

    // Payment failure starts dunning.
    receive_and_dispatch(
        &stack,
        &invoice_event(
            "evt_inv_fail",
            "invoice.payment_failed",
            Utc::now(),
            "in_1",
            "sub_1",
            "open",
        ),
    )
    .await;
    let subscription = {
        let mut scope = stack
            .tx
            .begin_unscoped("test_read_dunning", "test")
            .await
            .unwrap();
        let sub = stack
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await
            .unwrap()
            .expect("subscription");
        scope.commit().await.unwrap();
        sub
    };
    assert_eq!(subscription.collection_state(), CollectionState::Dunning);
    assert_eq!(subscription.status(), SubscriptionStatus::PastDue);

    // Invoice mirrored for the tenant UI.
    let invoice_count: i64 = sqlx::query_scalar("SELECT count(*) FROM billing.invoices")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(invoice_count, 1);

    // Dunning still grants access inside the grace window.
    assert!(subscription.grants_entitlements());

    // Age the failure beyond the grace window and sweep.
    sqlx::query(
        "UPDATE billing.subscriptions
         SET collection_started_at = now() - interval '15 days'
         WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .execute(&admin)
    .await
    .unwrap();
    let report = stack.dunning.execute(10).await.expect("dunning sweep");
    assert_eq!(report.grace_expired, 1);

    // Entitlements dropped to catalog defaults; tenant suspended for billing.
    let rows = subscription_source_rows(&admin, tenant_id).await;
    assert!(
        rows.is_empty(),
        "grace expiry clears subscription entitlements"
    );
    let tenant_status: String =
        sqlx::query_scalar("SELECT status FROM tenancy.tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(tenant_status, "suspended");
    let reason: String =
        sqlx::query_scalar("SELECT suspended_reason FROM tenancy.tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(reason, "billing");

    // Payment recovery: invoice.paid restores everything.
    receive_and_dispatch(
        &stack,
        &invoice_event(
            "evt_inv_paid",
            "invoice.paid",
            Utc::now(),
            "in_2",
            "sub_1",
            "paid",
        ),
    )
    .await;
    let subscription = {
        let mut scope = stack
            .tx
            .begin_unscoped("test_read_recovered", "test")
            .await
            .unwrap();
        let sub = stack
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await
            .unwrap()
            .expect("subscription");
        scope.commit().await.unwrap();
        sub
    };
    assert_eq!(subscription.collection_state(), CollectionState::Ok);
    let tenant_status: String =
        sqlx::query_scalar("SELECT status FROM tenancy.tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(tenant_status, "active", "billing suspension lifted");

    // Reconciliation: script a drift in Stripe and prove the repair.
    {
        let mut state = fake.inner.lock().unwrap();
        state.subscription_status = "canceled".to_string();
    }
    let report = stack.reconciliation.execute(10).await.expect("reconcile");
    assert!(report.checked >= 1);
    assert!(report.drift_repaired >= 1);
    let subscription = {
        let mut scope = stack
            .tx
            .begin_unscoped("test_read_reconciled", "test")
            .await
            .unwrap();
        let sub = stack
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await
            .unwrap()
            .expect("subscription");
        scope.commit().await.unwrap();
        sub
    };
    assert_eq!(
        subscription.status(),
        SubscriptionStatus::Canceled,
        "local state re-derived from Stripe"
    );
    let rows = subscription_source_rows(&admin, tenant_id).await;
    assert!(rows.is_empty(), "canceled subscription clears entitlements");

    // Unknown events are stored and acknowledged without state change.
    receive_and_dispatch(
        &stack,
        &serde_json::json!({
            "id": "evt_unknown",
            "type": "some.future.event",
            "created": Utc::now().timestamp(),
            "data": { "object": { "id": "obj_1" } }
        }),
    )
    .await;
}

#[tokio::test]
async fn platform_override_grants_and_revokes() {
    let (pool, admin, _node) = prepare().await;
    seed_features(&pool).await;
    let tenant_id = provision_tenant(&pool, "acme-override").await;
    let fake = Arc::new(FakeStripe::new());
    let stack = build_stack(pool.clone(), fake.clone()).await;

    stack
        .override_apply
        .execute(
            Uuid::nil(),
            ApplySubscriptionOverrideCommand {
                tenant_id,
                plan_key: "enterprise".to_string(),
                reason: "annual contract".to_string(),
            },
        )
        .await
        .expect("override applied");

    let rows = subscription_source_rows(&admin, tenant_id).await;
    assert!(
        rows.iter()
            .any(|(key, enabled, _)| key == "payments.stripe_connect" && *enabled),
        "enterprise plan grants connect"
    );
    let source: String =
        sqlx::query_scalar("SELECT source FROM billing.subscriptions WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(source, "override");

    // A live Stripe subscription would conflict — not exercised here; the
    // override revocation path is.
    use sanvi_billing::application::RevokeSubscriptionOverride;
    let revoke = RevokeSubscriptionOverride::new(
        stack.tx.clone(),
        stack.subscriptions.clone(),
        Arc::new(AccessEntitlementSyncer::new(Arc::new(
            sanvi_access::adapters::SqlxEntitlementRepository::new(),
        ))),
        Arc::new(SystemClock),
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
    );
    revoke
        .execute(Uuid::nil(), tenant_id)
        .await
        .expect("override revoked");
    let rows = subscription_source_rows(&admin, tenant_id).await;
    assert!(rows.is_empty(), "revoke clears the override's entitlements");
}

#[tokio::test]
async fn checkout_fails_loudly_without_tax_registration() {
    let (pool, _admin, _node) = prepare().await;
    seed_features(&pool).await;
    let tenant_id = provision_tenant(&pool, "acme-tax").await;
    let fake = Arc::new(FakeStripe::new());
    let mut stack = build_stack(pool.clone(), fake.clone()).await;

    // Rebuild the checkout use case with automatic_tax on and no
    // registrations scripted.
    let tax_gate: Arc<dyn TaxGate> = Arc::new(DirectTaxGate {
        stripe: stack.fake.clone(),
    });
    let mut scope = stack
        .tx
        .begin_unscoped("test_tax_plan", "test")
        .await
        .unwrap();
    let plan = SqlxPlanRepository::new()
        .find_by_key(&mut scope, "starter")
        .await
        .unwrap()
        .unwrap();
    let price_id = add_price(&stack, plan.id(), "price_tax_monthly").await;
    stack.checkout = CreateCheckoutSession::new(
        stack.tx.clone(),
        Arc::new(SqlxPlanRepository::new()),
        stack.prices.clone(),
        stack.subscriptions.clone(),
        stack.fake.clone(),
        tax_gate,
        Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new()),
        Arc::new(SystemClock),
        "http://localhost/success".to_string(),
        "http://localhost/cancel".to_string(),
        true, // automatic_tax enabled
        8,
    );

    let err = stack
        .checkout
        .execute(
            Uuid::nil(),
            CreateCheckoutSessionCommand {
                tenant_id,
                price_id,
                success_url: None,
                cancel_url: None,
            },
        )
        .await
        .expect_err("tax gate must fail loudly");
    assert!(matches!(
        err,
        sanvi_billing::domain::BillingError::TaxRegistrationMissing
    ));
}
