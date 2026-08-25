//! TASK-014 (slice 10.5) acceptance tests: conversion capture and the
//! directive gate, against a real Postgres.
//!
//! `DirectiveResolver`, `PaymentLookupPort` and `VendorRolePort` are faked
//! here on purpose — this suite proves `CaptureConversion`'s own gate
//! logic and the real `advertising.conversion_events` round-trip, not
//! phase 05's resolver or phase 09's payment tables. A separate small
//! suite below exercises the real cross-context `CheckoutPaymentLookup`
//! join, since that's where a currency/zero-decimal bug would actually
//! live.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use sanvi_advertising::adapters::{CheckoutPaymentLookup, SqlxCampaignRepository};
use sanvi_advertising::application::tracking::capture::{
    CaptureConversion, TrackConversionCommand,
};
use sanvi_advertising::domain::{
    ClickIds, Money, PaymentLookupPort, PlatformKey, VendorRole, VendorRolePort,
};
use sanvi_persistence::{DatabaseConfig, ScopedTx, TxManager, build_pool};
use sanvi_privacy::domain::{
    DirectiveResolver, DirectiveSnapshot, DirectiveSource, DirectiveState, JurisdictionRef,
    PrivacyError, ProcessingPurpose, PurposeDirective, SubjectRef,
};
use uuid::Uuid;

async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> Uuid {
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

/// A directive resolver whose answer per purpose is fixed in advance, plus
/// a default for anything not explicitly configured — mirrors the real
/// resolver's "never unknown" contract without needing the full privacy
/// schema.
struct FakeDirectiveResolver {
    answers: BTreeMap<ProcessingPurpose, (DirectiveState, DirectiveSource)>,
    default: (DirectiveState, DirectiveSource),
}

impl FakeDirectiveResolver {
    fn allow_all() -> Self {
        Self {
            answers: BTreeMap::new(),
            default: (DirectiveState::Allowed, DirectiveSource::Default),
        }
    }

    fn with(
        mut self,
        purpose: ProcessingPurpose,
        state: DirectiveState,
        source: DirectiveSource,
    ) -> Self {
        self.answers.insert(purpose, (state, source));
        self
    }
}

#[async_trait]
impl DirectiveResolver for FakeDirectiveResolver {
    async fn resolve(
        &self,
        _subject: &SubjectRef,
        purpose: ProcessingPurpose,
    ) -> Result<PurposeDirective, PrivacyError> {
        let (state, source) = self.answers.get(&purpose).copied().unwrap_or(self.default);
        Ok(PurposeDirective {
            purpose,
            state,
            source,
            jurisdiction: JurisdictionRef::from_static("eu"),
            notice_version: None,
            effective_at: Utc::now(),
            superseded_at: None,
        })
    }

    async fn snapshot(&self, subject: &SubjectRef) -> Result<DirectiveSnapshot, PrivacyError> {
        Ok(DirectiveSnapshot {
            subject: subject.clone(),
            jurisdiction: JurisdictionRef::from_static("eu"),
            directives: vec![],
            minor_opt_in_age: None,
            honours_universal_opt_out: false,
        })
    }
}

/// Mirrors the one thing about the real `DirectiveResolver` that matters
/// to capture's transaction shape: resolving takes a connection from this
/// same pool, once per purpose.
struct PoolTakingResolver {
    inner: FakeDirectiveResolver,
    tx: TxManager,
}

#[async_trait]
impl DirectiveResolver for PoolTakingResolver {
    async fn resolve(
        &self,
        subject: &SubjectRef,
        purpose: ProcessingPurpose,
    ) -> Result<PurposeDirective, PrivacyError> {
        let scope = self
            .tx
            .begin_unscoped("advertising_directive_gate", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let directive = self.inner.resolve(subject, purpose).await;
        drop(scope);
        directive
    }

    async fn snapshot(&self, subject: &SubjectRef) -> Result<DirectiveSnapshot, PrivacyError> {
        self.inner.snapshot(subject).await
    }
}

struct FixedPaymentLookup(Option<Money>);

#[async_trait]
impl PaymentLookupPort for FixedPaymentLookup {
    async fn lookup_payment_value(
        &self,
        _scope: &mut ScopedTx<'_>,
        _tenant_id: Uuid,
        _order_ref: &str,
    ) -> Result<Option<Money>, sanvi_advertising::domain::AdvertisingError> {
        Ok(self.0.clone())
    }
}

struct FixedVendorRole(VendorRole);

#[async_trait]
impl VendorRolePort for FixedVendorRole {
    async fn lookup_role(
        &self,
        _platform: &PlatformKey,
    ) -> Result<VendorRole, sanvi_advertising::domain::AdvertisingError> {
        Ok(self.0)
    }
}

fn capture_with(
    tx: TxManager,
    resolver: FakeDirectiveResolver,
    payment: Option<Money>,
    vendor_role: VendorRole,
) -> CaptureConversion {
    CaptureConversion::new(
        tx,
        Arc::new(SqlxCampaignRepository::new()),
        Arc::new(FixedPaymentLookup(payment)),
        Arc::new(FixedVendorRole(vendor_role)),
        Arc::new(resolver),
    )
}

fn base_command(tenant_id: Uuid, event_id: &str) -> TrackConversionCommand {
    TrackConversionCommand {
        tenant_id,
        event_name: "purchase".to_string(),
        event_id: event_id.to_string(),
        order_ref: None,
        click_ids: ClickIds::default(),
        value: None,
        currency: None,
        email: Some("A.User+tag@Example.COM".to_string()),
        phone: Some("+1 (415) 555-0100".to_string()),
        subject: SubjectRef::contact("customer-1@example.com"),
        platforms: vec![PlatformKey::parse("google_ads").unwrap()],
        platforms_incomplete: false,
    }
}

async fn fetch_row(
    pool: &sanvi_persistence::PgPool,
    tenant_id: Uuid,
    event_id: &str,
) -> serde_json::Value {
    use sqlx::Row;
    let manager = TxManager::new(pool.clone());
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    let row = sqlx::query(
        "SELECT id, event_id, value_minor, currency, value_source, click_ids, hashed_ids,
                consent, upload_state
         FROM advertising.conversion_events WHERE tenant_id = $1 AND event_id = $2",
    )
    .bind(tenant_id)
    .bind(event_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    serde_json::json!({
        "id": row.get::<Uuid, _>("id").to_string(),
        "value_minor": row.get::<Option<i64>, _>("value_minor"),
        "currency": row.get::<Option<String>, _>("currency"),
        "value_source": row.get::<Option<String>, _>("value_source"),
        "click_ids": row.get::<serde_json::Value, _>("click_ids"),
        "hashed_ids": row.get::<serde_json::Value, _>("hashed_ids"),
        "consent": row.get::<serde_json::Value, _>("consent"),
        "upload_state": row.get::<serde_json::Value, _>("upload_state"),
    })
}

async fn row_count(pool: &sanvi_persistence::PgPool, tenant_id: Uuid, event_id: &str) -> i64 {
    let manager = TxManager::new(pool.clone());
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM advertising.conversion_events WHERE tenant_id = $1 AND event_id = $2",
    )
    .bind(tenant_id)
    .bind(event_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap()
}

#[tokio::test]
async fn eu_subject_with_no_consent_is_suppressed() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "eu-no-consent").await;
    let tx = TxManager::new(pool.clone());

    let resolver = FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::AdsMeasurement,
        DirectiveState::Denied,
        DirectiveSource::Default,
    );
    let capture = capture_with(tx, resolver, None, VendorRole::ThirdParty);
    let command = base_command(tenant_id, "evt-eu-no-consent");
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-eu-no-consent").await;
    assert_eq!(row["upload_state"]["status"], "suppressed_by_directive");
    assert_eq!(row["upload_state"]["purpose"], "ads_measurement");
}

#[tokio::test]
async fn us_subject_with_no_signal_is_permitted() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "us-no-signal").await;
    let tx = TxManager::new(pool.clone());

    // Allowed everywhere, sourced from the jurisdiction default — exactly
    // "no explicit signal either way".
    let resolver = FakeDirectiveResolver::allow_all();
    let capture = capture_with(tx, resolver, None, VendorRole::ThirdParty);
    let command = base_command(tenant_id, "evt-us-no-signal");
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-us-no-signal").await;
    assert_eq!(row["upload_state"]["status"], "pending");
}

#[tokio::test]
async fn us_subject_opted_out_is_suppressed() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "us-opt-out").await;
    let tx = TxManager::new(pool.clone());

    let resolver = FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::SaleOrShare,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    );
    let capture = capture_with(tx, resolver, None, VendorRole::ThirdParty);
    let command = base_command(tenant_id, "evt-us-opt-out");
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-us-opt-out").await;
    assert_eq!(row["upload_state"]["status"], "suppressed_by_directive");
    assert_eq!(row["upload_state"]["purpose"], "sale_or_share");
    assert_eq!(row["upload_state"]["signal_source"], "opt_out");
}

#[tokio::test]
async fn gpc_signal_is_suppressed() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "gpc-signal").await;
    let tx = TxManager::new(pool.clone());

    let resolver = FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::TargetedAdvertising,
        DirectiveState::Denied,
        DirectiveSource::Signal,
    );
    let capture = capture_with(tx, resolver, None, VendorRole::ThirdParty);
    let command = base_command(tenant_id, "evt-gpc");
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-gpc").await;
    assert_eq!(row["upload_state"]["status"], "suppressed_by_directive");
    assert_eq!(row["upload_state"]["signal_source"], "signal");
}

#[tokio::test]
async fn vendor_role_changes_the_gate_outcome_for_the_same_event() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "vendor-role").await;

    let denied_sale_or_share = || {
        FakeDirectiveResolver::allow_all().with(
            ProcessingPurpose::SaleOrShare,
            DirectiveState::Denied,
            DirectiveSource::OptOut,
        )
    };

    // Service provider: sale_or_share is never asked, so the denial the
    // resolver would give doesn't matter — the event goes through.
    let tx = TxManager::new(pool.clone());
    let capture = capture_with(
        tx,
        denied_sale_or_share(),
        None,
        VendorRole::ServiceProvider,
    );
    capture
        .execute(base_command(tenant_id, "evt-service-provider"))
        .await
        .unwrap();
    let row = fetch_row(&pool, tenant_id, "evt-service-provider").await;
    assert_eq!(row["upload_state"]["status"], "pending");

    // Third party: the same denial now applies and suppresses the event.
    let tx = TxManager::new(pool.clone());
    let capture = capture_with(tx, denied_sale_or_share(), None, VendorRole::ThirdParty);
    capture
        .execute(base_command(tenant_id, "evt-third-party"))
        .await
        .unwrap();
    let row = fetch_row(&pool, tenant_id, "evt-third-party").await;
    assert_eq!(row["upload_state"]["status"], "suppressed_by_directive");
}

#[tokio::test]
async fn every_stored_event_carries_a_complete_consent_snapshot() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "consent-snapshot").await;
    let tx = TxManager::new(pool.clone());

    let capture = capture_with(
        tx,
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    capture
        .execute(base_command(tenant_id, "evt-consent-snapshot"))
        .await
        .unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-consent-snapshot").await;
    let consent = &row["consent"];
    assert!(!consent["resolver_version"].as_str().unwrap().is_empty());
    assert!(!consent["jurisdiction"].as_str().unwrap().is_empty());
    assert!(!consent["purposes_asked"].as_array().unwrap().is_empty());
    assert!(!consent["answers"].as_object().unwrap().is_empty());
    assert!(!consent["signal_source"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn no_raw_email_or_phone_is_ever_persisted() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "pii-scan").await;
    let tx = TxManager::new(pool.clone());

    let capture = capture_with(
        tx,
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    capture
        .execute(base_command(tenant_id, "evt-pii-scan"))
        .await
        .unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-pii-scan").await;
    let serialized = row.to_string();
    assert!(!serialized.to_lowercase().contains("a.user"));
    assert!(!serialized.contains("Example.COM"));
    assert!(!serialized.contains("4155550100"));
    assert!(!serialized.contains("555-0100"));

    // The normalization equivalence itself, proven end to end through the
    // stored hash rather than just the unit-level hashing function. Only
    // case and surrounding space are folded here: dots and `+tag` are
    // collapsed for Gmail addresses alone, and this fixture is not one.
    let hashed = row["hashed_ids"]["email"].as_str().unwrap().to_string();
    let via_plain_address =
        sanvi_advertising::domain::hashing::hash_email("a.user+tag@example.com");
    assert_eq!(hashed, via_plain_address.as_str());
    assert_ne!(
        hashed,
        sanvi_advertising::domain::hashing::hash_email("auser@example.com").as_str()
    );
}

#[tokio::test]
async fn client_value_is_overridden_by_the_payment_record_and_marked() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "value-override").await;
    let tx = TxManager::new(pool.clone());

    let real_payment = Money::new(4_980, "JPY").unwrap();
    let capture = capture_with(
        tx,
        FakeDirectiveResolver::allow_all(),
        Some(real_payment),
        VendorRole::ThirdParty,
    );

    let mut command = base_command(tenant_id, "evt-value-override");
    command.order_ref = Some("order-1".to_string());
    command.value = Some(1_000_000);
    command.currency = Some("JPY".to_string());
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-value-override").await;
    assert_eq!(row["value_minor"], 4_980);
    assert_eq!(row["currency"], "JPY");
    assert_eq!(row["value_source"], "payment_record");
}

#[tokio::test]
async fn same_event_id_posted_five_times_yields_one_row() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "dedupe").await;

    for _ in 0..5 {
        let tx = TxManager::new(pool.clone());
        let capture = capture_with(
            tx,
            FakeDirectiveResolver::allow_all(),
            None,
            VendorRole::ThirdParty,
        );
        capture
            .execute(base_command(tenant_id, "evt-dedupe"))
            .await
            .unwrap();
    }

    assert_eq!(row_count(&pool, tenant_id, "evt-dedupe").await, 1);
}

#[tokio::test]
async fn click_ids_present_partial_and_absent_are_distinguishable() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "click-ids").await;

    let cases = [
        (
            "evt-all-present",
            ClickIds {
                gclid: Some("g1".to_string()),
                gbraid: Some("gb1".to_string()),
                wbraid: Some("wb1".to_string()),
                fbc: Some("fbc1".to_string()),
                fbp: Some("fbp1".to_string()),
            },
        ),
        (
            "evt-partial",
            ClickIds {
                gclid: Some("g2".to_string()),
                ..ClickIds::default()
            },
        ),
        ("evt-absent", ClickIds::default()),
    ];

    for (event_id, click_ids) in cases {
        let tx = TxManager::new(pool.clone());
        let capture = capture_with(
            tx,
            FakeDirectiveResolver::allow_all(),
            None,
            VendorRole::ThirdParty,
        );
        let mut command = base_command(tenant_id, event_id);
        command.click_ids = click_ids;
        capture.execute(command).await.unwrap();
    }

    let all_present = fetch_row(&pool, tenant_id, "evt-all-present").await;
    assert_eq!(all_present["click_ids"]["gclid"], "g1");
    assert_eq!(all_present["click_ids"]["fbp"], "fbp1");

    let partial = fetch_row(&pool, tenant_id, "evt-partial").await;
    assert_eq!(partial["click_ids"]["gclid"], "g2");
    assert!(partial["click_ids"]["fbp"].is_null());

    let absent = fetch_row(&pool, tenant_id, "evt-absent").await;
    assert!(absent["click_ids"]["gclid"].is_null());
    assert!(absent["click_ids"]["fbp"].is_null());
}

/// Verifies the real cross-context join: `CheckoutPaymentLookup` reading
/// an actual `payments.checkouts` row, with no decimal arithmetic in the
/// path — the property that makes a zero-decimal currency like JPY carry
/// through correctly is "never touched", not "handled specially".
#[tokio::test]
async fn checkout_payment_lookup_joins_the_real_payments_schema() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "checkout-join").await;

    let seed_tx = TxManager::new(pool.clone());
    let mut seed_scope = seed_tx.begin_for(tenant_id).await.unwrap();

    let connection_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO payments.provider_connections
           (id, tenant_id, provider, external_account_id, status)
         VALUES ($1, $2, 'stripe_connect', 'acct_1', 'active')",
    )
    .bind(connection_id)
    .bind(tenant_id)
    .execute(&mut **seed_scope.tx_mut().as_inner())
    .await
    .unwrap();

    let checkout_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO payments.checkouts
           (id, tenant_id, connection_id, reference, amount_minor, currency, status, completed_at, created_at)
         VALUES ($1, $2, $3, 'order-jpy-1', 4980, 'JPY', 'complete', now(), now())",
    )
    .bind(checkout_id)
    .bind(tenant_id)
    .bind(connection_id)
    .execute(&mut **seed_scope.tx_mut().as_inner())
    .await
    .unwrap();
    seed_scope.commit().await.unwrap();

    let checkouts: Arc<dyn sanvi_payments::domain::CheckoutRepository> =
        Arc::new(sanvi_payments::adapters::SqlxCheckoutRepository::new());
    let lookup = CheckoutPaymentLookup::new(checkouts);

    let tx = TxManager::new(pool.clone());
    let mut scope = tx.begin_for(tenant_id).await.unwrap();
    let value = lookup
        .lookup_payment_value(&mut scope, tenant_id, "order-jpy-1")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(value.amount_minor, 4_980);
    assert_eq!(value.currency, "JPY");
}

/// Capture must resolve the gate *before* opening its own transaction. If
/// it holds one connection across a resolver that needs a second, enough
/// concurrent beacons take every connection in the pool and then wait for
/// a connection that only they could release — the pool deadlocks until
/// acquire timeouts unwind it, and `/public/track` loses the events behind
/// its unconditional `202`.
#[tokio::test]
async fn concurrent_captures_never_hold_a_connection_while_the_gate_needs_one() {
    let db = sanvi_testsupport::test_db().await;

    // Two connections, and more beacons in flight than that: with the
    // nested-transaction shape every one of them would be stuck.
    let pool = build_pool(&DatabaseConfig {
        max_connections: 2,
        min_connections: 1,
        ..db.app_config()
    })
    .await
    .unwrap();
    let tenant_id = seed_tenant(&pool, "gate-pool").await;
    let tx = TxManager::new(pool.clone());

    let capture = Arc::new(CaptureConversion::new(
        tx.clone(),
        Arc::new(SqlxCampaignRepository::new()),
        Arc::new(FixedPaymentLookup(None)),
        // ThirdParty asks all three purposes, so the resolver is entered
        // three times per event.
        Arc::new(FixedVendorRole(VendorRole::ThirdParty)),
        Arc::new(PoolTakingResolver {
            inner: FakeDirectiveResolver::allow_all(),
            tx,
        }),
    ));

    let mut handles = Vec::new();
    for i in 0..8 {
        let capture = capture.clone();
        handles.push(tokio::spawn(async move {
            capture
                .execute(base_command(tenant_id, &format!("evt-pool-{i}")))
                .await
        }));
    }

    for handle in handles {
        let outcome = handle.await.unwrap();
        outcome.expect("a capture failed to acquire a connection");
    }
}
