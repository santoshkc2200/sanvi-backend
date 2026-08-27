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

use actix_web::{App, http::StatusCode, test, web};
use async_trait::async_trait;
use chrono::Utc;
use sanvi_advertising::AdvertisingConfig;
use sanvi_advertising::adapters::{
    CheckoutPaymentLookup, OutboxEventPublisher, QuotaLimiter, SqlxAdvertisingConnectionRepository,
    SqlxCampaignRepository, SqlxConnectionTokenRepository, SqlxCreativeRepository,
    SqlxTenantEnvelopeCrypto, SqlxTrackingSettingsRepository, http::AdvertisingState,
};
use sanvi_advertising::application::tracking::{
    capture::{CaptureConversion, TrackConversionCommand},
    conversions::{GetConversionDiagnostics, ListConversions},
    settings::ManageTrackingSettings,
    site_key::generate_site_key,
};
use sanvi_advertising::application::{
    AddAd, AddAdGroup, CreateCampaign, CreateConnection, CreateCreative, DeleteConnection,
    DeleteCreative, GetCampaign, GetCreative, GetCreativePreviews, HandleCallback,
    ListCampaignChanges, ListCampaigns, ListConnections, ListCreatives, ListPlatformCatalog,
    PatchCampaign, PauseCampaign, PlatformRegistry, PublishCampaign, ReconcileCampaign,
    ResolveCampaignDrift, ResumeCampaign, StartAuthorization, UpdateAd, UpdateAdGroup,
    ValidateCampaign,
};
use sanvi_advertising::domain::{
    ClickIds, Money, OAuthStateManager, PaymentLookup, PaymentLookupPort, PlatformKey, TokenVault,
    VendorRole, VendorRolePort,
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

struct FixedPaymentLookup(Option<PaymentLookup>);

#[async_trait]
impl PaymentLookupPort for FixedPaymentLookup {
    async fn lookup_payment_value(
        &self,
        _scope: &mut ScopedTx<'_>,
        _tenant_id: Uuid,
        _order_ref: &str,
    ) -> Result<Option<PaymentLookup>, sanvi_advertising::domain::AdvertisingError> {
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
    payment: Option<PaymentLookup>,
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
                consent, upload_states
         FROM advertising.conversion_events WHERE tenant_id = $1 AND event_id = $2",
    )
    .bind(tenant_id)
    .bind(event_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    serde_json::json!({
        "id": row.get::<Uuid, _>("id").to_string(),
        "event_id": row.get::<String, _>("event_id"),
        "value_minor": row.get::<Option<i64>, _>("value_minor"),
        "currency": row.get::<Option<String>, _>("currency"),
        "value_source": row.get::<Option<String>, _>("value_source"),
        "click_ids": row.get::<serde_json::Value, _>("click_ids"),
        "hashed_ids": row.get::<serde_json::Value, _>("hashed_ids"),
        "consent": row.get::<serde_json::Value, _>("consent"),
        "upload_states": row.get::<serde_json::Value, _>("upload_states"),
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

async fn order_event_count(
    pool: &sanvi_persistence::PgPool,
    tenant_id: Uuid,
    order_ref: &str,
    event_name: Option<&str>,
) -> i64 {
    let manager = TxManager::new(pool.clone());
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM advertising.conversion_events
         WHERE tenant_id = $1 AND order_ref = $2
           AND ($3::text IS NULL OR name = $3)",
    )
    .bind(tenant_id)
    .bind(order_ref)
    .bind(event_name)
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
    assert_eq!(row["consent"]["answers"]["ads_measurement"], "denied");
    assert_eq!(row["consent"]["signal_source"], "default");
    assert_eq!(row["upload_states"], serde_json::json!({}));
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
    assert_eq!(row["consent"]["answers"]["ads_measurement"], "allowed");
    assert_eq!(row["upload_states"], serde_json::json!({}));
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
    assert_eq!(row["consent"]["answers"]["sale_or_share"], "denied");
    assert_eq!(row["consent"]["signal_source"], "opt_out");
    assert_eq!(row["upload_states"], serde_json::json!({}));
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
    assert_eq!(row["consent"]["answers"]["targeted_advertising"], "denied");
    assert_eq!(row["consent"]["signal_source"], "signal");
    assert_eq!(row["upload_states"], serde_json::json!({}));
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
    assert_eq!(row["consent"]["answers"]["ads_measurement"], "allowed");
    assert_eq!(row["consent"]["answers"].get("sale_or_share"), None);
    assert_eq!(row["upload_states"], serde_json::json!({}));

    // Third party: the same denial now applies and suppresses the event.
    let tx = TxManager::new(pool.clone());
    let capture = capture_with(tx, denied_sale_or_share(), None, VendorRole::ThirdParty);
    capture
        .execute(base_command(tenant_id, "evt-third-party"))
        .await
        .unwrap();
    let row = fetch_row(&pool, tenant_id, "evt-third-party").await;
    assert_eq!(row["consent"]["answers"]["sale_or_share"], "denied");
    assert_eq!(row["upload_states"], serde_json::json!({}));
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
    assert_eq!(consent["signal_source"], "default");
}

#[tokio::test]
async fn permitted_event_freezes_the_first_asked_purposes_signal_source() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "first-signal-source").await;
    let tx = TxManager::new(pool.clone());

    // Every purpose is allowed, but each answer comes from a different
    // source. `ads_measurement` is asked first, so its source is the one
    // frozen onto the event — the later purposes must not overwrite it.
    let resolver = FakeDirectiveResolver::allow_all()
        .with(
            ProcessingPurpose::AdsMeasurement,
            DirectiveState::Allowed,
            DirectiveSource::Signal,
        )
        .with(
            ProcessingPurpose::TargetedAdvertising,
            DirectiveState::Allowed,
            DirectiveSource::Consent,
        );
    let capture = capture_with(tx, resolver, None, VendorRole::ThirdParty);
    capture
        .execute(base_command(tenant_id, "evt-first-source"))
        .await
        .unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-first-source").await;
    assert_eq!(row["consent"]["answers"]["ads_measurement"], "allowed");
    assert_eq!(row["consent"]["answers"]["targeted_advertising"], "allowed");
    assert_eq!(row["consent"]["signal_source"], "signal");
}

#[tokio::test]
async fn no_raw_email_or_phone_is_ever_persisted() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "pii-scan").await;
    let secret = "test-signing-secret-of-at-least-32-bytes";
    let origin = "https://shop.example.test";
    let site_key = generate_site_key(secret, tenant_id, origin).unwrap();
    let state = create_test_advertising_state(pool.clone(), secret, 50, 50);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(sanvi_advertising::routes::tracking_public_routes),
    )
    .await;

    let request = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "Referer",
            "https://store.example.test/thank-you?order=123&email=a.user@example.com&phone=4155550100",
        ))
        .insert_header(("User-Agent", "Mozilla/5.0"))
        .peer_addr("198.51.100.9:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-pii-scan",
            "email": "A.User+tag@Example.COM",
            "phone": "+1 (415) 555-0100",
            "click_ids": {}
        }))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let row = fetch_row(&pool, tenant_id, "evt-pii-scan").await;
    let serialized = row.to_string();
    assert!(!serialized.to_lowercase().contains("a.user"));
    assert!(!serialized.contains("Example.COM"));
    assert!(!serialized.contains("4155550100"));
    assert!(!serialized.contains("555-0100"));
    assert!(!serialized.contains("order=123"));

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

    let payment_id = Uuid::now_v7();
    let real_payment = PaymentLookup {
        conversion_event_id: sanvi_payments::application::conversion_event_id(
            tenant_id, payment_id,
        )
        .to_string(),
        value: Money::new(4_980, "JPY").unwrap(),
    };
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

    let expected_event_id =
        sanvi_payments::application::conversion_event_id(tenant_id, payment_id).to_string();

    let row = fetch_row(&pool, tenant_id, &expected_event_id).await;
    assert_eq!(row["value_minor"], 4_980);
    assert_eq!(row["currency"], "JPY");
    assert_eq!(row["value_source"], "payment_record");
    assert_eq!(row["event_id"], expected_event_id);
}

#[tokio::test]
async fn server_derived_event_id_deduplicates_different_client_event_ids_for_same_order() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "server-event-id").await;
    let tx = TxManager::new(pool.clone());

    let payment_id = Uuid::now_v7();
    let real_payment = PaymentLookup {
        conversion_event_id: sanvi_payments::application::conversion_event_id(
            tenant_id, payment_id,
        )
        .to_string(),
        value: Money::new(4_980, "JPY").unwrap(),
    };
    let capture = capture_with(
        tx,
        FakeDirectiveResolver::allow_all(),
        Some(real_payment),
        VendorRole::ThirdParty,
    );

    let mut command1 = base_command(tenant_id, "client-evt-1");
    command1.order_ref = Some("order-dedupe-1".to_string());
    capture.execute(command1).await.unwrap();

    let mut command2 = base_command(tenant_id, "client-evt-2");
    command2.order_ref = Some("order-dedupe-1".to_string());
    capture.execute(command2).await.unwrap();

    let expected_event_id =
        sanvi_payments::application::conversion_event_id(tenant_id, payment_id).to_string();

    let row = fetch_row(&pool, tenant_id, &expected_event_id).await;
    assert_eq!(row["event_id"], expected_event_id);
    assert_eq!(row["value_minor"], 4_980);
    assert_eq!(row_count(&pool, tenant_id, &expected_event_id).await, 1);
    assert_eq!(row_count(&pool, tenant_id, "client-evt-1").await, 0);
    assert_eq!(row_count(&pool, tenant_id, "client-evt-2").await, 0);
}

#[tokio::test]
async fn order_dedupe_survives_the_payment_webhook_race() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "webhook-race-dedupe").await;
    let tx = TxManager::new(pool.clone());

    let before_webhook = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let mut first = base_command(tenant_id, "client-before-webhook");
    first.order_ref = Some("order-webhook-race".to_string());
    before_webhook.execute(first).await.unwrap();

    let payment_id = Uuid::now_v7();
    let after_webhook = capture_with(
        tx,
        FakeDirectiveResolver::allow_all(),
        Some(PaymentLookup {
            conversion_event_id: sanvi_payments::application::conversion_event_id(
                tenant_id, payment_id,
            )
            .to_string(),
            value: Money::new(4_980, "JPY").unwrap(),
        }),
        VendorRole::ThirdParty,
    );
    let mut retry = base_command(tenant_id, "server-after-webhook");
    retry.order_ref = Some("order-webhook-race".to_string());
    after_webhook.execute(retry).await.unwrap();

    assert_eq!(
        order_event_count(&pool, tenant_id, "order-webhook-race", Some("purchase")).await,
        1
    );
}

#[tokio::test]
async fn server_derived_event_id_keeps_distinct_event_names() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "event-name-dedupe").await;
    let tx = TxManager::new(pool.clone());
    let payment_id = Uuid::now_v7();
    let payment = PaymentLookup {
        conversion_event_id: sanvi_payments::application::conversion_event_id(
            tenant_id, payment_id,
        )
        .to_string(),
        value: Money::new(4_980, "JPY").unwrap(),
    };
    let capture = capture_with(
        tx,
        FakeDirectiveResolver::allow_all(),
        Some(payment),
        VendorRole::ThirdParty,
    );

    let mut purchase = base_command(tenant_id, "client-purchase");
    purchase.order_ref = Some("order-multiple-events".to_string());
    capture.execute(purchase).await.unwrap();

    let mut start_trial = base_command(tenant_id, "client-start-trial");
    start_trial.event_name = "start_trial".to_string();
    start_trial.order_ref = Some("order-multiple-events".to_string());
    capture.execute(start_trial).await.unwrap();

    assert_eq!(
        order_event_count(&pool, tenant_id, "order-multiple-events", Some("purchase")).await,
        1
    );
    assert_eq!(
        order_event_count(
            &pool,
            tenant_id,
            "order-multiple-events",
            Some("start_trial")
        )
        .await,
        1
    );
    assert_eq!(
        order_event_count(&pool, tenant_id, "order-multiple-events", None).await,
        2
    );
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
                capture_time: None,
                landing_url: None,
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

/// Verifies the real cross-context join: `CheckoutPaymentLookup` reads the
/// actual `payments.payments` row even before the asynchronously-updated
/// checkout status reaches `complete`. Its conversion id must therefore use
/// `PaymentRecord.id`, never the distinct checkout id.
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
           (id, tenant_id, connection_id, reference, amount_minor, currency, status, created_at)
         VALUES ($1, $2, $3, 'order-jpy-1', 4980, 'JPY', 'open', now())",
    )
    .bind(checkout_id)
    .bind(tenant_id)
    .bind(connection_id)
    .execute(&mut **seed_scope.tx_mut().as_inner())
    .await
    .unwrap();

    let payment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO payments.payments
           (id, tenant_id, checkout_id, external_payment_id, status, amount_minor, currency)
         VALUES ($1, $2, $3, 'pi_jpy_1', 'processing', 4980, 'JPY')",
    )
    .bind(payment_id)
    .bind(tenant_id)
    .bind(checkout_id)
    .execute(&mut **seed_scope.tx_mut().as_inner())
    .await
    .unwrap();
    seed_scope.commit().await.unwrap();

    let checkouts: Arc<dyn sanvi_payments::domain::CheckoutRepository> =
        Arc::new(sanvi_payments::adapters::SqlxCheckoutRepository::new());
    let lookup = CheckoutPaymentLookup::new(checkouts);

    let tx = TxManager::new(pool.clone());
    let mut scope = tx.begin_for(tenant_id).await.unwrap();
    let payment = lookup
        .lookup_payment_value(&mut scope, tenant_id, "order-jpy-1")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        payment.conversion_event_id,
        sanvi_payments::application::conversion_event_id(tenant_id, payment_id).to_string()
    );
    assert_eq!(payment.value.amount_minor, 4_980);
    assert_eq!(payment.value.currency, "JPY");
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

fn create_test_advertising_state(
    pool: sanvi_persistence::PgPool,
    secret: &str,
    ip_permits: usize,
    domain_permits: usize,
) -> AdvertisingState {
    let tx = TxManager::new(pool.clone());
    let campaign_repo = Arc::new(SqlxCampaignRepository::new());
    let payment_lookup = Arc::new(FixedPaymentLookup(None));
    let vendor_role = Arc::new(FixedVendorRole(VendorRole::ThirdParty));
    let directive_resolver = Arc::new(FakeDirectiveResolver::allow_all());

    let capture = Arc::new(CaptureConversion::new(
        tx.clone(),
        campaign_repo.clone(),
        payment_lookup,
        vendor_role,
        directive_resolver,
    ));

    let settings_repo = Arc::new(SqlxTrackingSettingsRepository::new());
    let settings_mgr = Arc::new(ManageTrackingSettings::new(tx.clone(), settings_repo));
    let list_conversions = Arc::new(ListConversions::new(tx.clone(), campaign_repo.clone()));
    let get_conversion_diagnostics = Arc::new(GetConversionDiagnostics::new(
        tx.clone(),
        campaign_repo.clone(),
    ));

    let config = AdvertisingConfig {
        tracking_site_key_signing_secret: secret.to_string(),
        ..Default::default()
    };

    let oauth_state = OAuthStateManager::new("test-oauth-secret-of-at-least-32-bytes");

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    let registry = Arc::new(PlatformRegistry::new());
    let events = Arc::new(OutboxEventPublisher::new());
    let campaigns = campaign_repo.clone();
    let token_vault = Arc::new(TokenVault::new(
        Arc::new(SqlxTenantEnvelopeCrypto::new(
            [0u8; 32],
            "test-kek".to_string(),
        )),
        Arc::new(SqlxConnectionTokenRepository::new()),
    ));
    let creatives = Arc::new(SqlxCreativeRepository::new());

    AdvertisingState {
        config,
        idempotency: sanvi_persistence::IdempotencyStore::new(pool),
        catalog: Arc::new(ListPlatformCatalog::new(registry.clone())),
        reconcile_campaign: Arc::new(ReconcileCampaign::new(
            tx.clone(),
            campaigns.clone(),
            events.clone(),
        )),
        resolve_campaign_drift: Arc::new(ResolveCampaignDrift::new(
            tx.clone(),
            campaigns.clone(),
            events.clone(),
            registry.clone(),
        )),
        create_campaign: Arc::new(CreateCampaign::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            registry.clone(),
            events.clone(),
        )),
        get_campaign: Arc::new(GetCampaign::new(tx.clone(), campaigns.clone())),
        list_campaigns: Arc::new(ListCampaigns::new(tx.clone(), campaigns.clone())),
        validate_campaign: Arc::new(ValidateCampaign::new(
            tx.clone(),
            campaigns.clone(),
            creatives.clone(),
            registry.clone(),
        )),
        patch_campaign: Arc::new(PatchCampaign::new(
            tx.clone(),
            campaigns.clone(),
            creatives.clone(),
            events.clone(),
            registry.clone(),
        )),
        publish_campaign: Arc::new(PublishCampaign::new(
            tx.clone(),
            campaigns.clone(),
            creatives.clone(),
            events.clone(),
            registry.clone(),
        )),
        pause_campaign: Arc::new(PauseCampaign::new(
            tx.clone(),
            campaigns.clone(),
            events.clone(),
            registry.clone(),
        )),
        resume_campaign: Arc::new(ResumeCampaign::new(
            tx.clone(),
            campaigns.clone(),
            events.clone(),
            registry.clone(),
        )),
        list_campaign_changes: Arc::new(ListCampaignChanges::new(tx.clone(), campaigns.clone())),
        add_ad_group: Arc::new(AddAdGroup::new(
            tx.clone(),
            campaigns.clone(),
            events.clone(),
            registry.clone(),
        )),
        update_ad_group: Arc::new(UpdateAdGroup::new(
            tx.clone(),
            campaigns.clone(),
            events.clone(),
            registry.clone(),
        )),
        add_ad: Arc::new(AddAd::new(
            tx.clone(),
            campaigns.clone(),
            creatives.clone(),
            events.clone(),
            registry.clone(),
        )),
        update_ad: Arc::new(UpdateAd::new(
            tx.clone(),
            campaigns.clone(),
            creatives.clone(),
            events.clone(),
            registry.clone(),
        )),
        start_authorization: Arc::new(StartAuthorization::new(
            registry.clone(),
            oauth_state.clone(),
        )),
        handle_callback: Arc::new(HandleCallback::new(
            tx.clone(),
            connections.clone(),
            registry.clone(),
            oauth_state.clone(),
            token_vault,
        )),
        create_connection: Arc::new(CreateConnection::new(tx.clone(), connections.clone())),
        list_connections: Arc::new(ListConnections::new(
            tx.clone(),
            connections.clone(),
            registry.clone(),
        )),
        delete_connection: Arc::new(DeleteConnection::new(tx.clone(), connections.clone())),
        create_creative: Arc::new(CreateCreative::new(
            tx.clone(),
            creatives.clone(),
            connections,
            registry.clone(),
        )),
        get_creative: Arc::new(GetCreative::new(tx.clone(), creatives.clone())),
        list_creatives: Arc::new(ListCreatives::new(tx.clone(), creatives.clone())),
        delete_creative: Arc::new(DeleteCreative::new(tx.clone(), creatives.clone())),
        get_creative_previews: Arc::new(GetCreativePreviews::new(tx, creatives, registry)),
        oauth_state,
        tracking_ip_limiter: Arc::new(QuotaLimiter::new(100, ip_permits)),
        tracking_domain_limiter: Arc::new(QuotaLimiter::new(100, domain_permits)),
        capture,
        settings_mgr,
        list_conversions,
        get_conversion_diagnostics,
    }
}

#[tokio::test]
async fn http_click_ids_capture_extraction_and_merging() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "click-ids-http").await;
    let secret = "test-signing-secret-of-at-least-32-bytes";
    let origin = "https://store.example.test";
    let site_key = generate_site_key(secret, tenant_id, origin).unwrap();

    let state = create_test_advertising_state(pool.clone(), secret, 50, 50);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(sanvi_advertising::routes::tracking_public_routes),
    )
    .await;

    // 1. Body only
    let req1 = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.10:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-body-only",
            "click_ids": {
                "gclid": "g_body",
                "gbraid": "gb_body",
                "wbraid": "wb_body",
                "fbc": "fbc_body",
                "fbp": "fbp_body",
                "capture_time": "2026-08-26T00:00:00Z",
                "landing_url": "https://store.example.test/landing?email=a.user@example.com"
            }
        }))
        .to_request();
    let res1 = test::call_service(&app, req1).await;
    assert_eq!(res1.status(), StatusCode::ACCEPTED);
    let body1 = test::read_body(res1).await;
    assert_eq!(body1, "{}");

    let row1 = fetch_row(&pool, tenant_id, "evt-body-only").await;
    assert_eq!(row1["click_ids"]["gclid"], "g_body");
    assert_eq!(row1["click_ids"]["gbraid"], "gb_body");
    assert_eq!(row1["click_ids"]["wbraid"], "wb_body");
    assert_eq!(row1["click_ids"]["fbc"], "fbc_body");
    assert_eq!(row1["click_ids"]["fbp"], "fbp_body");
    assert_eq!(row1["click_ids"]["capture_time"], "2026-08-26T00:00:00Z");
    assert_eq!(
        row1["click_ids"]["landing_url"],
        "https://store.example.test/landing"
    );

    // 2. Referer + cookies only
    let referer2 = "https://store.example.test/checkout?gclid=EAIaIQobCh%3D%3D&gbraid=gb+ref&wbraid=wb_ref#details";
    let req2 = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header(("Referer", referer2))
        .insert_header(("Cookie", "_fbc=fbc_cookie; _fbp=fbp_cookie"))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.11:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-server-only",
            "click_ids": {}
        }))
        .to_request();
    let res2 = test::call_service(&app, req2).await;
    assert_eq!(res2.status(), StatusCode::ACCEPTED);
    let body2 = test::read_body(res2).await;
    assert_eq!(body2, "{}");

    let row2 = fetch_row(&pool, tenant_id, "evt-server-only").await;
    assert_eq!(row2["click_ids"]["gclid"], "EAIaIQobCh==");
    assert_eq!(row2["click_ids"]["gbraid"], "gb ref");
    assert_eq!(row2["click_ids"]["wbraid"], "wb_ref");
    assert_eq!(row2["click_ids"]["fbc"], "fbc_cookie");
    assert_eq!(row2["click_ids"]["fbp"], "fbp_cookie");
    assert!(!row2["click_ids"]["capture_time"].is_null());
    assert_eq!(
        row2["click_ids"]["landing_url"],
        "https://store.example.test/checkout"
    );

    // 3. Mixed: server values win over conflicting body values; fallback to body where server missing
    let referer3 = "https://store.example.test/cart?gclid=g_server_win&gbraid=gb_server_win";
    let req3 = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header(("Referer", referer3))
        .insert_header(("Cookie", "_fbc=fbc_server_win"))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.12:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-mixed",
            "click_ids": {
                "gclid": "g_body_lose",
                "wbraid": "wb_body_fallback",
                "fbc": "fbc_body_lose",
                "fbp": "fbp_body_fallback"
            }
        }))
        .to_request();
    let res3 = test::call_service(&app, req3).await;
    assert_eq!(res3.status(), StatusCode::ACCEPTED);
    let body3 = test::read_body(res3).await;
    assert_eq!(body3, "{}");

    let row3 = fetch_row(&pool, tenant_id, "evt-mixed").await;
    assert_eq!(row3["click_ids"]["gclid"], "g_server_win");
    assert_eq!(row3["click_ids"]["gbraid"], "gb_server_win");
    assert_eq!(row3["click_ids"]["wbraid"], "wb_body_fallback");
    assert_eq!(row3["click_ids"]["fbc"], "fbc_server_win");
    assert_eq!(row3["click_ids"]["fbp"], "fbp_body_fallback");
    assert!(!row3["click_ids"]["capture_time"].is_null());
    assert_eq!(
        row3["click_ids"]["landing_url"],
        "https://store.example.test/cart"
    );

    // 4. Wholly absent
    let req4 = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.13:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-wholly-absent",
            "click_ids": {}
        }))
        .to_request();
    let res4 = test::call_service(&app, req4).await;
    assert_eq!(res4.status(), StatusCode::ACCEPTED);
    let body4 = test::read_body(res4).await;
    assert_eq!(body4, "{}");

    let row4 = fetch_row(&pool, tenant_id, "evt-wholly-absent").await;
    assert!(row4["click_ids"]["gclid"].is_null());
    assert!(row4["click_ids"]["gbraid"].is_null());
    assert!(row4["click_ids"]["wbraid"].is_null());
    assert!(row4["click_ids"]["fbc"].is_null());
    assert!(row4["click_ids"]["fbp"].is_null());
    assert!(row4["click_ids"]["landing_url"].is_null());
    assert!(!row4["click_ids"]["capture_time"].is_null());
}

#[tokio::test]
async fn dry_run_test_event_does_not_populate_server_click_ids() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "dry-run-click-ids").await;
    let tx = TxManager::new(pool.clone());

    let capture = capture_with(
        tx,
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );

    let command = base_command(tenant_id, "evt-dry-run");
    let dry_run_event = capture.dry_run(command).await.unwrap();

    assert_eq!(dry_run_event.click_ids.capture_time, None);
    assert_eq!(dry_run_event.click_ids.landing_url, None);
    assert_eq!(dry_run_event.click_ids.gclid, None);
}

#[tokio::test]
async fn http_abuse_control_payload_over_limit_returns_202_and_stores_nothing() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "abuse-oversized").await;
    let secret = "test-signing-secret-of-at-least-32-bytes";
    let origin = "https://shop.example.test";
    let site_key = generate_site_key(secret, tenant_id, origin).unwrap();

    let state = create_test_advertising_state(pool.clone(), secret, 50, 50);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(sanvi_advertising::routes::tracking_public_routes),
    )
    .await;

    // >16KiB payload
    let large_event_name = "x".repeat(17 * 1024);
    let payload = serde_json::json!({
        "event_name": large_event_name,
        "event_id": "evt-oversized"
    });

    let req = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .insert_header(("Content-Type", "application/json"))
        .peer_addr("198.51.100.20:1234".parse().unwrap())
        .set_payload(payload.to_string())
        .to_request();

    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = test::read_body(res).await;
    assert_eq!(body, "{}");

    assert_eq!(row_count(&pool, tenant_id, "evt-oversized").await, 0);
}

#[tokio::test]
async fn http_abuse_control_missing_or_mismatched_origin_returns_202_and_stores_nothing() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "abuse-origin").await;
    let secret = "test-signing-secret-of-at-least-32-bytes";
    let origin = "https://shop.example.test";
    let site_key = generate_site_key(secret, tenant_id, origin).unwrap();

    let state = create_test_advertising_state(pool.clone(), secret, 50, 50);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(sanvi_advertising::routes::tracking_public_routes),
    )
    .await;

    // 1. Missing Origin
    let req_missing = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.21:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-missing-origin"
        }))
        .to_request();

    let res_missing = test::call_service(&app, req_missing).await;
    assert_eq!(res_missing.status(), StatusCode::ACCEPTED);
    let body_missing = test::read_body(res_missing).await;
    assert_eq!(body_missing, "{}");
    assert_eq!(row_count(&pool, tenant_id, "evt-missing-origin").await, 0);

    // 2. Mismatched Origin
    let req_mismatched = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", "https://evil.example.test"))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.22:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-mismatched-origin"
        }))
        .to_request();

    let res_mismatched = test::call_service(&app, req_mismatched).await;
    assert_eq!(res_mismatched.status(), StatusCode::ACCEPTED);
    let body_mismatched = test::read_body(res_mismatched).await;
    assert_eq!(body_mismatched, "{}");
    assert_eq!(
        row_count(&pool, tenant_id, "evt-mismatched-origin").await,
        0
    );
}

#[tokio::test]
async fn http_abuse_control_invalid_or_forged_site_key_returns_202_and_stores_nothing() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "abuse-site-key").await;
    let secret = "test-signing-secret-of-at-least-32-bytes";
    let origin = "https://shop.example.test";

    let state = create_test_advertising_state(pool.clone(), secret, 50, 50);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(sanvi_advertising::routes::tracking_public_routes),
    )
    .await;

    // 1. Completely bogus site key
    let req_bogus = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", "not-a-valid-key"))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.23:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-bogus-key"
        }))
        .to_request();

    let res_bogus = test::call_service(&app, req_bogus).await;
    assert_eq!(res_bogus.status(), StatusCode::ACCEPTED);
    let body_bogus = test::read_body(res_bogus).await;
    assert_eq!(body_bogus, "{}");
    assert_eq!(row_count(&pool, tenant_id, "evt-bogus-key").await, 0);

    // 2. Forged signature minted with wrong secret
    let forged_key = generate_site_key(
        "wrong-secret-signing-secret-at-least-32-bytes",
        tenant_id,
        origin,
    )
    .unwrap();
    let req_forged = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", forged_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.24:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-forged-key"
        }))
        .to_request();

    let res_forged = test::call_service(&app, req_forged).await;
    assert_eq!(res_forged.status(), StatusCode::ACCEPTED);
    let body_forged = test::read_body(res_forged).await;
    assert_eq!(body_forged, "{}");
    assert_eq!(row_count(&pool, tenant_id, "evt-forged-key").await, 0);
}

#[tokio::test]
async fn http_abuse_control_rate_limit_breach_returns_202_and_stores_nothing() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "abuse-ratelimit").await;
    let secret = "test-signing-secret-of-at-least-32-bytes";
    let origin = "https://shop.example.test";
    let site_key = generate_site_key(secret, tenant_id, origin).unwrap();

    // 1 per-IP permit, 10 per-domain permits
    let state = create_test_advertising_state(pool.clone(), secret, 1, 10);

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state.clone()))
            .configure(sanvi_advertising::routes::tracking_public_routes),
    )
    .await;

    // 1. IP rate limiter: Hold the single permit for IP 198.51.100.99
    let ip_permit = state.tracking_ip_limiter.try_acquire("198.51.100.99");
    assert!(ip_permit.is_some());

    // Request from 198.51.100.99 trips the rate limit
    let req_ip_limited = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.99:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-ip-limited"
        }))
        .to_request();

    let res_ip_limited = test::call_service(&app, req_ip_limited).await;
    assert_eq!(res_ip_limited.status(), StatusCode::ACCEPTED);
    let body_ip_limited = test::read_body(res_ip_limited).await;
    assert_eq!(body_ip_limited, "{}");
    assert_eq!(row_count(&pool, tenant_id, "evt-ip-limited").await, 0);

    // Release the permit
    drop(ip_permit);

    // Request from 198.51.100.99 now succeeds and stores the row
    let req_ip_ok = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.99:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-ip-ok"
        }))
        .to_request();

    let res_ip_ok = test::call_service(&app, req_ip_ok).await;
    assert_eq!(res_ip_ok.status(), StatusCode::ACCEPTED);
    let body_ip_ok = test::read_body(res_ip_ok).await;
    assert_eq!(body_ip_ok, "{}");
    assert_eq!(row_count(&pool, tenant_id, "evt-ip-ok").await, 1);

    // 2. Domain rate limiter: Hold all permits for origin
    let mut domain_permits = Vec::new();
    while let Some(permit) = state.tracking_domain_limiter.try_acquire(origin) {
        domain_permits.push(permit);
    }
    assert!(!domain_permits.is_empty());

    // Request with origin trips domain limit
    let req_domain_limited = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.100:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-domain-limited"
        }))
        .to_request();

    let res_domain_limited = test::call_service(&app, req_domain_limited).await;
    assert_eq!(res_domain_limited.status(), StatusCode::ACCEPTED);
    let body_domain_limited = test::read_body(res_domain_limited).await;
    assert_eq!(body_domain_limited, "{}");
    assert_eq!(row_count(&pool, tenant_id, "evt-domain-limited").await, 0);

    // Release domain permits
    drop(domain_permits);

    // Request now succeeds and stores the row
    let req_domain_ok = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header(("Origin", origin))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        ))
        .peer_addr("198.51.100.100:1234".parse().unwrap())
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": "evt-domain-ok"
        }))
        .to_request();

    let res_domain_ok = test::call_service(&app, req_domain_ok).await;
    assert_eq!(res_domain_ok.status(), StatusCode::ACCEPTED);
    let body_domain_ok = test::read_body(res_domain_ok).await;
    assert_eq!(body_domain_ok, "{}");
    assert_eq!(row_count(&pool, tenant_id, "evt-domain-ok").await, 1);
}
