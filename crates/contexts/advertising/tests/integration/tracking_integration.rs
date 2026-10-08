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

use actix_web::{App, HttpMessage, http::StatusCode, test, web};
use async_trait::async_trait;
use chrono::Utc;
use sanvi_advertising::AdvertisingConfig;
use sanvi_advertising::adapters::SystemIdGenerator;
use sanvi_advertising::adapters::{
    AdvertisingMetrics, CheckoutPaymentLookup, FakeAdPlatform, FakeAudienceSync,
    FakeConversionSink, OutboxEventPublisher, QuotaLimiter, SqlxAdvertisingConnectionRepository,
    SqlxCampaignRepository, SqlxConnectionTokenRepository, SqlxCreativeRepository,
    SqlxMetricsRepository, SqlxTenantEnvelopeCrypto, SqlxTrackingSettingsRepository,
    http::AdvertisingState,
};
use sanvi_advertising::application::tracking::{
    CreateAudience, ListAudiences, RefreshAudience, RetractWithdrawnConversions, SyncAudiences,
    UploadPendingConversions,
    capture::{CaptureConversion, TrackConversionCommand},
    conversions::{GetConversionDiagnostics, ListConversions},
    retry::RetryConversion,
    settings::ManageTrackingSettings,
    site_key::generate_site_key,
};
use sanvi_advertising::application::{
    AddAd, AddAdGroup, CreateCampaign, CreateConnection, CreateCreative, DeleteConnection,
    DeleteCreative, GetCampaign, GetCreative, GetCreativePreviews, GetFreshness, HandleCallback,
    ListCampaignChanges, ListCampaigns, ListConnections, ListCreatives, ListPlatformCatalog,
    PatchCampaign, PauseCampaign, PlatformRegistry, PollCampaignReviewStatus, PublishCampaign,
    QueryMetrics, ReconcileCampaign, ResolveCampaignDrift, ResumeCampaign, StartAuthorization,
    UpdateAd, UpdateAdGroup, ValidateCampaign,
};
use sanvi_advertising::domain::{
    AdvertisingConnection, AdvertisingConnectionRepository, AdvertisingError, AudienceId,
    AudienceStatus, AudienceSyncPort, ClickIds, ConnectionId, ConversionEventRepository,
    ConversionId, ConversionSinkPort, Money, OAuthStateManager, PaymentLookup, PaymentLookupPort,
    PlatformKey, SecretRef, TenantTimezoneSource, TokenVault, UploadState, VendorRole,
    VendorRolePort,
};
use sanvi_kernel::TenantId;
use sanvi_persistence::{DatabaseConfig, ScopedTx, TxManager, build_pool};
use sanvi_privacy::domain::{
    DirectiveResolver, DirectiveSnapshot, DirectiveSource, DirectiveState, JurisdictionRef,
    PrivacyError, ProcessingPurpose, PurposeDirective, SubjectRef,
};

async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> TenantId {
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

/// Fixed tenant timezone for the tracking harness (mirrors the seeded
/// `Asia/Tokyo` connections).
#[derive(Debug, Clone, Copy, Default)]
struct HarnessTimezones;

#[async_trait]
impl TenantTimezoneSource for HarnessTimezones {
    async fn timezone(
        &self,
        _scope: &mut ScopedTx<'_>,
        _tenant_id: TenantId,
    ) -> Result<String, AdvertisingError> {
        Ok("Asia/Tokyo".to_string())
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
        _tenant_id: TenantId,
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
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    )
}

fn base_command(tenant_id: TenantId, event_id: &str) -> TrackConversionCommand {
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

/// A `Contact` (email-identified) subject stores no reconstructable key
/// material (TASK-014's no-raw-PII rule), so its retry withdrawal re-check
/// always refuses — see `retry_conversion_contact_subject_cannot_retry`.
/// Retry tests exercising the actual directive re-check need a
/// device-identified subject instead.
fn command_with_device_subject(tenant_id: TenantId, event_id: &str) -> TrackConversionCommand {
    TrackConversionCommand {
        subject: SubjectRef::tenant_device(tenant_id, "device-1"),
        ..base_command(tenant_id, event_id)
    }
}

async fn fetch_row(
    pool: &sanvi_persistence::PgPool,
    tenant_id: TenantId,
    event_id: &str,
) -> serde_json::Value {
    use sqlx::Row;
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let row = sqlx::query(
        "SELECT id, event_id, value_minor, currency, value_source, click_ids, hashed_ids,
                consent, upload_states
         FROM advertising.conversion_events WHERE tenant_id = $1 AND event_id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(event_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    serde_json::json!({
        "id": row.get::<i64, _>("id").to_string(),
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

async fn row_count(pool: &sanvi_persistence::PgPool, tenant_id: TenantId, event_id: &str) -> i64 {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM advertising.conversion_events WHERE tenant_id = $1 AND event_id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(event_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap()
}

async fn order_event_count(
    pool: &sanvi_persistence::PgPool,
    tenant_id: TenantId,
    order_ref: &str,
    event_name: Option<&str>,
) -> i64 {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM advertising.conversion_events
         WHERE tenant_id = $1 AND order_ref = $2
           AND ($3::text IS NULL OR name = $3)",
    )
    .bind(tenant_id.as_i64())
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    // Allowed everywhere, sourced from the jurisdiction default — exactly
    // "no explicit signal either way".
    let resolver = FakeDirectiveResolver::allow_all();
    let capture = capture_with(tx, resolver, None, VendorRole::ThirdParty);
    let command = base_command(tenant_id, "evt-us-no-signal");
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-us-no-signal").await;
    assert_eq!(row["consent"]["answers"]["ads_measurement"], "allowed");
    assert_eq!(
        row["upload_states"],
        serde_json::json!({
            "google_ads": {
                "status": "pending",
                "attempt_count": 0
            }
        })
    );
}

#[tokio::test]
async fn us_subject_opted_out_is_suppressed() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "us-opt-out").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
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
    assert_eq!(
        row["upload_states"],
        serde_json::json!({
            "google_ads": {
                "status": "pending",
                "attempt_count": 0
            }
        })
    );

    // Third party: the same denial now applies and suppresses the event.
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let payment_id = sanvi_payments::domain::payment::PaymentId::from_snowflake(
        sanvi_testsupport::test_ids().next_id().unwrap(),
    );
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let payment_id = sanvi_payments::domain::payment::PaymentId::from_snowflake(
        sanvi_testsupport::test_ids().next_id().unwrap(),
    );
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let before_webhook = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let mut first = base_command(tenant_id, "client-before-webhook");
    first.order_ref = Some("order-webhook-race".to_string());
    before_webhook.execute(first).await.unwrap();

    let payment_id = sanvi_payments::domain::payment::PaymentId::from_snowflake(
        sanvi_testsupport::test_ids().next_id().unwrap(),
    );
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let payment_id = sanvi_payments::domain::payment::PaymentId::from_snowflake(
        sanvi_testsupport::test_ids().next_id().unwrap(),
    );
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
        let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
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
        let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
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

    let seed_tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut seed_scope = seed_tx.begin_for(tenant_id.as_i64()).await.unwrap();

    let connection_id = sanvi_payments::domain::connection::ConnectionId::from_snowflake(
        sanvi_testsupport::test_ids().next_id().unwrap(),
    );
    sqlx::query(
        "INSERT INTO payments.provider_connections
           (id, tenant_id, provider, external_account_id, status)
         VALUES ($1, $2, 'stripe_connect', 'acct_1', 'active')",
    )
    .bind(connection_id.as_i64())
    .bind(tenant_id.as_i64())
    .execute(&mut **seed_scope.tx_mut().as_inner())
    .await
    .unwrap();

    let checkout_id = sanvi_payments::domain::checkout::CheckoutId::from_snowflake(
        sanvi_testsupport::test_ids().next_id().unwrap(),
    );
    sqlx::query(
        "INSERT INTO payments.checkouts
           (id, tenant_id, connection_id, reference, amount_minor, currency, status, created_at)
         VALUES ($1, $2, $3, 'order-jpy-1', 4980, 'JPY', 'open', now())",
    )
    .bind(checkout_id.as_i64())
    .bind(tenant_id.as_i64())
    .bind(connection_id.as_i64())
    .execute(&mut **seed_scope.tx_mut().as_inner())
    .await
    .unwrap();

    let payment_id = sanvi_payments::domain::payment::PaymentId::from_snowflake(
        sanvi_testsupport::test_ids().next_id().unwrap(),
    );
    sqlx::query(
        "INSERT INTO payments.payments
           (id, tenant_id, checkout_id, external_payment_id, status, amount_minor, currency)
         VALUES ($1, $2, $3, 'pi_jpy_1', 'processing', 4980, 'JPY')",
    )
    .bind(payment_id.as_i64())
    .bind(tenant_id.as_i64())
    .bind(checkout_id.as_i64())
    .execute(&mut **seed_scope.tx_mut().as_inner())
    .await
    .unwrap();
    seed_scope.commit().await.unwrap();

    let checkouts: Arc<dyn sanvi_payments::domain::CheckoutRepository> =
        Arc::new(sanvi_payments::adapters::SqlxCheckoutRepository::new());
    let lookup = CheckoutPaymentLookup::new(checkouts);

    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

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
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let campaign_repo = Arc::new(SqlxCampaignRepository::new());
    let payment_lookup = Arc::new(FixedPaymentLookup(None));
    let vendor_role = Arc::new(FixedVendorRole(VendorRole::ThirdParty));
    let directive_resolver = Arc::new(FakeDirectiveResolver::allow_all());

    let capture = Arc::new(CaptureConversion::new(
        tx.clone(),
        campaign_repo.clone(),
        payment_lookup,
        vendor_role,
        directive_resolver.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    ));

    let settings_repo = Arc::new(SqlxTrackingSettingsRepository::new());
    let settings_mgr = Arc::new(ManageTrackingSettings::new(tx.clone(), settings_repo));
    let list_conversions = Arc::new(ListConversions::new(tx.clone(), campaign_repo.clone()));
    let get_conversion_diagnostics = Arc::new(GetConversionDiagnostics::new(
        tx.clone(),
        campaign_repo.clone(),
    ));
    let retry_conversion = Arc::new(RetryConversion::new(
        tx.clone(),
        campaign_repo.clone(),
        directive_resolver.clone(),
    ));

    let config = AdvertisingConfig {
        tracking_site_key_signing_secret: sanvi_telemetry::Redacted::new(secret.to_string()),
        ..Default::default()
    };

    let oauth_state = OAuthStateManager::new("test-oauth-secret-of-at-least-32-bytes");

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    let registry = Arc::new(PlatformRegistry::new());
    let events = Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let campaigns = campaign_repo.clone();
    let token_vault = Arc::new(TokenVault::new(
        Arc::new(SqlxTenantEnvelopeCrypto::new(
            [0u8; 32],
            "test-kek".to_string(),
        )),
        Arc::new(SqlxConnectionTokenRepository::new()),
    ));
    let creatives = Arc::new(SqlxCreativeRepository::new());
    let fx_rates: Arc<dyn sanvi_advertising::domain::FxRatePort> =
        Arc::new(sanvi_advertising::adapters::UnavailableFxRate::new());

    AdvertisingState {
        config,
        idempotency: sanvi_persistence::IdempotencyStore::new(pool),
        catalog: Arc::new(ListPlatformCatalog::new(registry.clone())),
        reconcile_campaign: Arc::new(ReconcileCampaign::new(
            tx.clone(),
            campaigns.clone(),
            events.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        resolve_campaign_drift: Arc::new(ResolveCampaignDrift::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        create_campaign: Arc::new(CreateCampaign::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            registry.clone(),
            events.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            creatives.clone(),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        publish_campaign: Arc::new(PublishCampaign::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            creatives.clone(),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        pause_campaign: Arc::new(PauseCampaign::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        resume_campaign: Arc::new(ResumeCampaign::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        list_campaign_changes: Arc::new(ListCampaignChanges::new(tx.clone(), campaigns.clone())),
        add_ad_group: Arc::new(AddAdGroup::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        update_ad_group: Arc::new(UpdateAdGroup::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        add_ad: Arc::new(AddAd::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            creatives.clone(),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        poll_review_status: Arc::new(PollCampaignReviewStatus::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            registry.clone(),
        )),
        update_ad: Arc::new(UpdateAd::new(
            tx.clone(),
            campaigns.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            creatives.clone(),
            events.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        start_authorization: Arc::new(StartAuthorization::new(
            registry.clone(),
            oauth_state.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        handle_callback: Arc::new(HandleCallback::new(
            tx.clone(),
            connections.clone(),
            registry.clone(),
            oauth_state.clone(),
            token_vault,
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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
            connections.clone(),
            registry.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        get_creative: Arc::new(GetCreative::new(tx.clone(), creatives.clone())),
        list_creatives: Arc::new(ListCreatives::new(tx.clone(), creatives.clone())),
        delete_creative: Arc::new(DeleteCreative::new(tx.clone(), creatives.clone())),
        get_creative_previews: Arc::new(GetCreativePreviews::new(
            tx.clone(),
            creatives.clone(),
            registry.clone(),
        )),
        create_audience: Arc::new(CreateAudience::new(
            tx.clone(),
            campaign_repo.clone(),
            Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
        )),
        list_audiences: Arc::new(ListAudiences::new(tx.clone(), campaign_repo.clone())),
        refresh_audience: {
            let mut audience_sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>> =
                BTreeMap::new();
            for descriptor in FakeAdPlatform::fixtures() {
                audience_sinks.insert(
                    descriptor.key.clone(),
                    Arc::new(FakeAudienceSync::new(descriptor)),
                );
            }
            Arc::new(RefreshAudience::new(
                tx.clone(),
                campaign_repo.clone(),
                campaign_repo.clone(),
                connections.clone(),
                directive_resolver,
                audience_sinks,
                Arc::new(AdvertisingMetrics::default()),
            ))
        },
        query_metrics: Arc::new(QueryMetrics::new(
            tx.clone(),
            Arc::new(SqlxMetricsRepository::new()),
            92,
            7,
        )),
        get_freshness: Arc::new(GetFreshness::new(tx.clone(), connections.clone(), 26.0)),
        get_budget_caps: Arc::new(sanvi_advertising::application::GetBudgetCaps::new(
            tx.clone(),
            Arc::new(sanvi_advertising::adapters::SqlxBudgetCapRepository::new()),
        )),
        put_budget_cap: {
            let caps_repo = Arc::new(sanvi_advertising::adapters::SqlxBudgetCapRepository::new());
            let status_service = Arc::new(sanvi_advertising::application::GetSpendStatus::new(
                tx.clone(),
                Arc::new(sanvi_advertising::adapters::SqlxMetricsRepository::new()),
                caps_repo.clone(),
                campaign_repo.clone(),
                connections.clone(),
                Arc::new(sanvi_advertising::adapters::UnavailableFxRate::new()),
                Arc::new(HarnessTimezones),
                7,
            ));
            let pause = Arc::new(sanvi_advertising::application::PauseCampaign::new(
                tx.clone(),
                campaign_repo.clone(),
                Arc::new(SqlxAdvertisingConnectionRepository::new()),
                events.clone(),
                registry.clone(),
                Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
            ));
            let evaluator = Arc::new(sanvi_advertising::application::ThresholdEvaluator::new(
                tx.clone(),
                caps_repo.clone(),
                Arc::new(sanvi_advertising::adapters::SqlxBudgetAlertRepository::new()),
                campaign_repo.clone(),
                pause,
                status_service.clone(),
                events.clone(),
                Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
                Arc::new(AdvertisingMetrics::default()),
            ));
            Arc::new(sanvi_advertising::application::PutBudgetCap::new(
                tx.clone(),
                caps_repo,
                Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
                status_service,
                evaluator,
            ))
        },
        get_campaign_budget_cap: Arc::new(
            sanvi_advertising::application::GetCampaignBudgetCap::new(
                tx.clone(),
                Arc::new(sanvi_advertising::adapters::SqlxBudgetCapRepository::new()),
            ),
        ),
        list_budget_alerts: Arc::new(sanvi_advertising::application::ListBudgetAlerts::new(
            tx.clone(),
            Arc::new(sanvi_advertising::adapters::SqlxBudgetAlertRepository::new()),
        )),
        acknowledge_budget_alert: Arc::new(
            sanvi_advertising::application::AcknowledgeBudgetAlert::new(
                tx.clone(),
                Arc::new(sanvi_advertising::adapters::SqlxBudgetAlertRepository::new()),
            ),
        ),
        get_spend_status: Arc::new(sanvi_advertising::application::GetSpendStatus::new(
            tx.clone(),
            Arc::new(sanvi_advertising::adapters::SqlxMetricsRepository::new()),
            Arc::new(sanvi_advertising::adapters::SqlxBudgetCapRepository::new()),
            campaign_repo.clone(),
            connections.clone(),
            fx_rates.clone(),
            Arc::new(HarnessTimezones),
            7,
        )),
        fx_rates: fx_rates.clone(),
        get_platform_ads_health: Arc::new(
            sanvi_advertising::application::GetPlatformAdsHealth::new(
                tx.clone(),
                connections.clone(),
                registry.clone(),
                6.0,
            ),
        ),
        oauth_state,
        tracking_ip_limiter: Arc::new(QuotaLimiter::new(100, ip_permits)),
        tracking_domain_limiter: Arc::new(QuotaLimiter::new(100, domain_permits)),
        capture,
        settings_mgr,
        list_conversions,
        get_conversion_diagnostics,
        retry_conversion,
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
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

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

#[tokio::test]
async fn retry_conversion_parked_to_pending_transition_succeeds() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retry-parked-ok").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let resolver = Arc::new(FakeDirectiveResolver::allow_all());
    let repo = Arc::new(SqlxCampaignRepository::new());
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-retry-parked");
    capture.execute(command).await.unwrap();

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let events = repo.list(&mut scope, tenant_id).await.unwrap();
    assert_eq!(events.len(), 1);
    let conversion_id = events[0].id;

    let google_ads = PlatformKey::parse("google_ads").unwrap();
    let meta = PlatformKey::parse("meta").unwrap();
    let mut upload_states = BTreeMap::new();
    upload_states.insert(
        google_ads.clone(),
        UploadState::Parked {
            reason: "exhausted retries".to_string(),
            attempts: 3,
        },
    );
    upload_states.insert(meta.clone(), UploadState::Uploaded { attempt_count: 1 });
    repo.update_upload_states(&mut scope, tenant_id, conversion_id, upload_states)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let retry = RetryConversion::new(tx.clone(), repo.clone(), resolver);
    let retried_event = retry.execute(tenant_id, conversion_id).await.unwrap();

    assert_eq!(
        retried_event.upload_states.get(&google_ads),
        Some(&UploadState::Pending { attempt_count: 4 })
    );
    assert_eq!(
        retried_event.upload_states.get(&meta),
        Some(&UploadState::Uploaded { attempt_count: 1 })
    );

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let reloaded = repo
        .find_by_id(&mut scope, tenant_id, conversion_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.upload_states.get(&google_ads),
        Some(&UploadState::Pending { attempt_count: 4 })
    );
    assert_eq!(
        reloaded.upload_states.get(&meta),
        Some(&UploadState::Uploaded { attempt_count: 1 })
    );
}

#[tokio::test]
async fn retry_conversion_non_parked_event_is_rejected() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retry-non-parked").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let resolver = Arc::new(FakeDirectiveResolver::allow_all());
    let repo = Arc::new(SqlxCampaignRepository::new());
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = base_command(tenant_id, "evt-retry-non-parked");
    capture.execute(command).await.unwrap();

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let events = repo.list(&mut scope, tenant_id).await.unwrap();
    let conversion_id = events[0].id;
    scope.commit().await.unwrap();

    let retry = RetryConversion::new(tx.clone(), repo.clone(), resolver);

    // Case 1: Empty / initial upload states -> rejected
    let err = retry.execute(tenant_id, conversion_id).await.unwrap_err();
    assert!(matches!(err, AdvertisingError::InvalidInput(_)));

    // Case 2: Only Uploaded state -> rejected
    let google_ads = PlatformKey::parse("google_ads").unwrap();
    let mut upload_states = BTreeMap::new();
    upload_states.insert(
        google_ads.clone(),
        UploadState::Uploaded { attempt_count: 1 },
    );
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.update_upload_states(&mut scope, tenant_id, conversion_id, upload_states)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let err = retry.execute(tenant_id, conversion_id).await.unwrap_err();
    assert!(matches!(err, AdvertisingError::InvalidInput(_)));

    // Case 3: Only transient Failed (not Parked) state -> rejected
    let mut upload_states = BTreeMap::new();
    upload_states.insert(
        google_ads.clone(),
        UploadState::Failed {
            reason: "transient network error".to_string(),
            attempt_count: 1,
        },
    );
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.update_upload_states(&mut scope, tenant_id, conversion_id, upload_states)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let err = retry.execute(tenant_id, conversion_id).await.unwrap_err();
    assert!(matches!(err, AdvertisingError::InvalidInput(_)));

    // Case 4: Non-existent event -> ConversionNotFound
    let non_existent_id = ConversionId::try_from(9_999_999_999_i64).unwrap();
    let err = retry.execute(tenant_id, non_existent_id).await.unwrap_err();
    assert!(matches!(err, AdvertisingError::ConversionNotFound));
}

#[tokio::test]
async fn retry_conversion_directive_withdrawn_is_rejected_even_if_parked() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retry-withdrawn").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let repo = Arc::new(SqlxCampaignRepository::new());
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-retry-withdrawn");
    capture.execute(command).await.unwrap();

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let events = repo.list(&mut scope, tenant_id).await.unwrap();
    let conversion_id = events[0].id;

    let google_ads = PlatformKey::parse("google_ads").unwrap();
    let mut upload_states = BTreeMap::new();
    upload_states.insert(
        google_ads.clone(),
        UploadState::Parked {
            reason: "exhausted retries".to_string(),
            attempts: 3,
        },
    );
    repo.update_upload_states(&mut scope, tenant_id, conversion_id, upload_states)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // Directive now Denied (consent was withdrawn)
    let denying_resolver = Arc::new(FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::AdsMeasurement,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    ));

    let retry = RetryConversion::new(tx.clone(), repo.clone(), denying_resolver);
    let err = retry.execute(tenant_id, conversion_id).await.unwrap_err();
    assert!(matches!(err, AdvertisingError::InvalidInput(msg) if msg.contains("withdrawn")));

    // State in DB remains Parked (not modified)
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let unchanged = repo
        .find_by_id(&mut scope, tenant_id, conversion_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        unchanged.upload_states.get(&google_ads),
        Some(UploadState::Parked { attempts: 3, .. })
    ));
}

#[tokio::test]
async fn retry_conversion_double_retry_is_idempotent() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retry-idempotent").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let resolver = Arc::new(FakeDirectiveResolver::allow_all());
    let repo = Arc::new(SqlxCampaignRepository::new());
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-retry-idempotent");
    capture.execute(command).await.unwrap();

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let events = repo.list(&mut scope, tenant_id).await.unwrap();
    let conversion_id = events[0].id;

    let google_ads = PlatformKey::parse("google_ads").unwrap();
    let mut upload_states = BTreeMap::new();
    upload_states.insert(
        google_ads.clone(),
        UploadState::Parked {
            reason: "exhausted retries".to_string(),
            attempts: 2,
        },
    );
    repo.update_upload_states(&mut scope, tenant_id, conversion_id, upload_states)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let retry = RetryConversion::new(tx.clone(), repo.clone(), resolver);

    // First retry call: flips Parked -> Pending { attempt_count: 3 }
    let first_result = retry.execute(tenant_id, conversion_id).await.unwrap();
    assert_eq!(
        first_result.upload_states.get(&google_ads),
        Some(&UploadState::Pending { attempt_count: 3 })
    );

    // Second retry call immediately: no-op success returning the event unchanged
    let second_result = retry.execute(tenant_id, conversion_id).await.unwrap();
    assert_eq!(
        second_result.upload_states.get(&google_ads),
        Some(&UploadState::Pending { attempt_count: 3 })
    );
    assert_eq!(first_result.upload_states, second_result.upload_states);
}

/// An event captured under an email-identified subject stores no
/// reconstructable key material (TASK-014 forbids persisting the raw
/// email here). Retry must refuse rather than re-check withdrawal against
/// a fabricated subject that was never the one the gate actually asked —
/// regression test for that exact bug.
#[tokio::test]
async fn retry_conversion_contact_subject_cannot_retry() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retry-contact-subject").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let resolver = Arc::new(FakeDirectiveResolver::allow_all());
    let repo = Arc::new(SqlxCampaignRepository::new());
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    // base_command's subject is `SubjectRef::contact(..)` — email-identified.
    let command = base_command(tenant_id, "evt-retry-contact-subject");
    capture.execute(command).await.unwrap();

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let events = repo.list(&mut scope, tenant_id).await.unwrap();
    let conversion_id = events[0].id;

    let google_ads = PlatformKey::parse("google_ads").unwrap();
    let mut upload_states = BTreeMap::new();
    upload_states.insert(
        google_ads.clone(),
        UploadState::Parked {
            reason: "exhausted retries".to_string(),
            attempts: 1,
        },
    );
    repo.update_upload_states(&mut scope, tenant_id, conversion_id, upload_states)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let retry = RetryConversion::new(tx.clone(), repo.clone(), resolver);
    let err = retry.execute(tenant_id, conversion_id).await.unwrap_err();
    assert!(matches!(
        err,
        AdvertisingError::InvalidInput(msg) if msg.contains("email-identified")
    ));

    // State in DB remains Parked (not silently flipped to Pending).
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let unchanged = repo
        .find_by_id(&mut scope, tenant_id, conversion_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        unchanged.upload_states.get(&google_ads),
        Some(UploadState::Parked { attempts: 1, .. })
    ));
}

async fn seed_test_connection(
    tx: &TxManager,
    connections: &SqlxAdvertisingConnectionRepository,
    tenant_id: TenantId,
    platform: &str,
) -> ConnectionId {
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        PlatformKey::parse(platform).unwrap(),
        "123-456-7890",
        "USD",
        "America/New_York",
        SecretRef::new("vault-ref"),
        None,
        Utc::now(),
    )
    .unwrap();
    let connection_id = connection.id();
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    connections.upsert(&mut scope, &connection).await.unwrap();
    scope.commit().await.unwrap();
    connection_id
}

#[tokio::test]
async fn upload_sweep_permitted_event_gets_pending_at_capture() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "upload-permitted-pending").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let mut command = command_with_device_subject(tenant_id, "evt-upload-permitted");
    command.platforms = vec![
        PlatformKey::parse("google_ads").unwrap(),
        PlatformKey::parse("meta").unwrap(),
    ];
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-upload-permitted").await;
    assert_eq!(
        row["upload_states"],
        serde_json::json!({
            "google_ads": {
                "status": "pending",
                "attempt_count": 0
            },
            "meta": {
                "status": "pending",
                "attempt_count": 0
            }
        })
    );
}

#[tokio::test]
async fn upload_sweep_suppressed_event_gets_empty_upload_states_and_is_untouched_by_worker() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "upload-suppressed-empty").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[0].clone(),
    ));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );

    let denied_resolver = FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::AdsMeasurement,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    );
    let capture = capture_with(tx.clone(), denied_resolver, None, VendorRole::ThirdParty);
    let command = command_with_device_subject(tenant_id, "evt-suppressed-capture");
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-suppressed-capture").await;
    assert_eq!(row["upload_states"], serde_json::json!({}));

    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let sweep_resolver = Arc::new(FakeDirectiveResolver::allow_all());
    let worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        sweep_resolver,
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    let report = worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 0);
    assert_eq!(report.uploaded, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(report.parked, 0);
    assert_eq!(report.suppressed_late, 0);
    assert_eq!(google_sink.uploaded_events().len(), 0);

    let row_after = fetch_row(&pool, tenant_id, "evt-suppressed-capture").await;
    assert_eq!(row_after["upload_states"], serde_json::json!({}));
}

#[tokio::test]
async fn upload_sweep_uploads_pending_event_and_flips_to_uploaded() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "upload-pending-uploaded").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[0].clone(),
    ));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );

    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-upload-ok");
    capture.execute(command).await.unwrap();

    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let resolver = Arc::new(FakeDirectiveResolver::allow_all());
    let worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        resolver,
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    let report = worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.uploaded, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(report.parked, 0);
    assert_eq!(report.suppressed_late, 0);

    assert_eq!(google_sink.uploaded_events().len(), 1);
    assert_eq!(google_sink.uploaded_events()[0].event_id, "evt-upload-ok");

    let row = fetch_row(&pool, tenant_id, "evt-upload-ok").await;
    assert_eq!(
        row["upload_states"],
        serde_json::json!({
            "google_ads": {
                "status": "uploaded",
                "attempt_count": 0
            }
        })
    );
}

#[tokio::test]
async fn upload_sweep_withdrawn_directive_produces_failed_suppressed_late_without_sink_call() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "upload-withdrawn-suppressed-late").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[0].clone(),
    ));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );

    // Permitted at capture time
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-withdrawn-later");
    capture.execute(command).await.unwrap();

    let row = fetch_row(&pool, tenant_id, "evt-withdrawn-later").await;
    assert_eq!(
        row["upload_states"],
        serde_json::json!({
            "google_ads": {
                "status": "pending",
                "attempt_count": 0
            }
        })
    );

    // Directive is now Denied (withdrawn since capture)
    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let withdrawn_resolver = Arc::new(FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::AdsMeasurement,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    ));
    let worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        withdrawn_resolver,
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    let report = worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.uploaded, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(report.parked, 0);
    assert_eq!(report.suppressed_late, 1);

    // Sink was NEVER called
    assert_eq!(google_sink.uploaded_events().len(), 0);

    let row = fetch_row(&pool, tenant_id, "evt-withdrawn-later").await;
    assert_eq!(row["upload_states"]["google_ads"]["status"], "failed");
    assert!(
        row["upload_states"]["google_ads"]["reason"]
            .as_str()
            .unwrap()
            .contains("suppressed_late")
    );
}

#[tokio::test]
async fn upload_sweep_parks_event_after_5_consecutive_failures() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "upload-parked-after-5").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sink = Arc::new(
        FakeConversionSink::new(FakeAdPlatform::fixtures()[0].clone()).with_upload_error(
            AdvertisingError::PlatformUnavailable("503 Service Unavailable".to_string()),
        ),
    );
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );

    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-fail-to-park");
    capture.execute(command).await.unwrap();

    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let resolver = Arc::new(FakeDirectiveResolver::allow_all());
    let worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        resolver,
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    // Attempts 1 to 4: state is Failed
    for attempt in 1..=4 {
        let report = worker.execute(100).await.unwrap();
        assert_eq!(report.checked, 1, "attempt {attempt} checked");
        assert_eq!(report.uploaded, 0, "attempt {attempt} uploaded");
        assert_eq!(report.failed, 1, "attempt {attempt} failed");
        assert_eq!(report.parked, 0, "attempt {attempt} parked");

        let row = fetch_row(&pool, tenant_id, "evt-fail-to-park").await;
        assert_eq!(row["upload_states"]["google_ads"]["status"], "failed");
        assert_eq!(row["upload_states"]["google_ads"]["attempt_count"], attempt);
    }

    // Attempt 5: reaches bounded ceiling -> Parked
    let report = worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 1, "attempt 5 checked");
    assert_eq!(report.uploaded, 0, "attempt 5 uploaded");
    assert_eq!(report.failed, 0, "attempt 5 failed");
    assert_eq!(report.parked, 1, "attempt 5 parked");

    let row = fetch_row(&pool, tenant_id, "evt-fail-to-park").await;
    assert_eq!(row["upload_states"]["google_ads"]["status"], "parked");
    assert_eq!(row["upload_states"]["google_ads"]["attempts"], 5);

    // Attempt 6: Parked event is not picked up anymore
    let report = worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 0);
    assert_eq!(report.uploaded, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(report.parked, 0);
}

/// TASK-023 (FR-1112/FR-1113), dependency-matrix rows "Google Ads"/"Meta"
/// outage: a failing platform parks the conversion with its directive
/// snapshot intact, never drops it, and never uploads without the
/// snapshot; the healthy platform is untouched.
#[tokio::test]
async fn upload_sweep_outage_parks_conversions_with_their_directive_snapshot() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "outage-parks-with-snapshot").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    let fixtures = FakeAdPlatform::fixtures();
    // Fixture 0 is google_ads, fixture 1 is meta (platform catalog order).
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;
    seed_test_connection(&tx, &connections, tenant_id, "meta").await;

    let google_sink = Arc::new(FakeConversionSink::new(fixtures[0].clone()));
    let meta_sink = Arc::new(
        FakeConversionSink::new(fixtures[1].clone()).with_upload_error(
            AdvertisingError::PlatformUnavailable("simulated meta outage".to_string()),
        ),
    );
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );
    sinks.insert(PlatformKey::parse("meta").unwrap(), meta_sink.clone());

    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    capture
        .execute({
            let mut command = command_with_device_subject(tenant_id, "evt-outage-snapshot");
            // Both platforms must be pending: the sweep only attempts what
            // capture marked, so a google-only command would never prove
            // anything about the meta outage.
            command.platforms = vec![
                PlatformKey::parse("google_ads").unwrap(),
                PlatformKey::parse("meta").unwrap(),
            ];
            command
        })
        .await
        .unwrap();

    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    // Sweep five times: meta fails each time, then parks at the ceiling.
    for attempt in 1..=5 {
        let report = worker.execute(100).await.unwrap();
        assert_eq!(report.failed + report.parked, 1, "attempt {attempt}");
        assert_eq!(report.uploaded, 1, "google uploads on attempt {attempt}");
    }

    // Meta parked; google uploaded.
    let row = fetch_row(&pool, tenant_id, "evt-outage-snapshot").await;
    assert_eq!(row["upload_states"]["meta"]["status"], "parked");
    assert_eq!(row["upload_states"]["google_ads"]["status"], "uploaded");

    // The parked conversion still carries its directive snapshot: the
    // later-justifiable upload is possible because the snapshot travelled
    // with the event, not because anyone re-asked.
    let consent = &row["consent"];
    assert!(
        !consent["resolver_version"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    );
    assert!(
        !consent["purposes_asked"]
            .as_array()
            .expect("purposes frozen")
            .is_empty()
    );

    // Nothing ever uploaded without a snapshot: every upload the healthy
    // sink accepted carried the frozen directive.
    for (_, event) in google_sink.uploaded_records() {
        assert!(
            !event.consent.resolver_version.is_empty() && !event.consent.purposes_asked.is_empty(),
            "no upload may happen without its directive snapshot"
        );
    }

    // The event was never dropped: the row persists, ready for the
    // operator replay path (RetryConversion).
    assert_eq!(row_count(&pool, tenant_id, "evt-outage-snapshot").await, 1);
}

#[tokio::test]
async fn upload_sweep_observability_metrics_recorded_accurately() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "upload-observability-metrics").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;
    seed_test_connection(&tx, &connections, tenant_id, "meta").await;

    let google_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[0].clone(),
    ));
    let meta_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[1].clone(),
    ));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );
    sinks.insert(PlatformKey::parse("meta").unwrap(), meta_sink.clone());

    let registry = prometheus::Registry::new();
    let metrics = Arc::new(AdvertisingMetrics::new(&registry));

    // Capture an event destined for both google_ads and meta
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = TrackConversionCommand {
        tenant_id,
        event_name: "purchase".to_string(),
        event_id: "evt-metrics-test".to_string(),
        order_ref: None,
        subject: SubjectRef::tenant_device(tenant_id, "dev-metrics"),
        email: None,
        phone: None,
        value: Some(5000),
        currency: Some("USD".to_string()),
        click_ids: ClickIds::default(),
        platforms: vec![
            PlatformKey::parse("google_ads").unwrap(),
            PlatformKey::parse("meta").unwrap(),
        ],
        platforms_incomplete: false,
    };
    capture.execute(command).await.unwrap();

    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let resolver = Arc::new(FakeDirectiveResolver::allow_all());
    let worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        resolver,
        sinks.clone(),
        metrics.clone(),
    );

    let report = worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.uploaded, 2);

    // Verify upload metrics for both platforms
    assert_eq!(
        metrics
            .conversion_upload_total()
            .with_label_values(&["google_ads", "uploaded"])
            .get(),
        1
    );
    assert_eq!(
        metrics
            .conversion_upload_total()
            .with_label_values(&["meta", "uploaded"])
            .get(),
        1
    );
    assert_eq!(
        metrics.conversion_upload_lag_seconds().get_sample_count(),
        2
    );
    assert_eq!(metrics.conversion_dlq_depth().get(), 0);

    // Test suppressed_late metrics
    let withdrawn_resolver = Arc::new(FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::AdsMeasurement,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    ));
    let command_late = TrackConversionCommand {
        tenant_id,
        event_name: "purchase".to_string(),
        event_id: "evt-metrics-late".to_string(),
        order_ref: None,
        subject: SubjectRef::tenant_device(tenant_id, "dev-late"),
        email: None,
        phone: None,
        value: Some(1000),
        currency: Some("USD".to_string()),
        click_ids: ClickIds::default(),
        platforms: vec![PlatformKey::parse("google_ads").unwrap()],
        platforms_incomplete: false,
    };
    capture.execute(command_late).await.unwrap();

    let worker_withdrawn = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        withdrawn_resolver,
        sinks.clone(),
        metrics.clone(),
    );
    let report_late = worker_withdrawn.execute(100).await.unwrap();
    assert_eq!(report_late.suppressed_late, 1);
    assert_eq!(
        metrics
            .conversion_upload_total()
            .with_label_values(&["google_ads", "suppressed_late"])
            .get(),
        1
    );
    assert_eq!(
        metrics
            .conversion_suppressed_total()
            .with_label_values(&["ads_measurement", "opt_out"])
            .get(),
        1
    );

    // Test failed and parked metrics
    meta_sink.set_upload_error(Some(AdvertisingError::PlatformUnavailable(
        "error".to_string(),
    )));
    let command_fail = TrackConversionCommand {
        tenant_id,
        event_name: "purchase".to_string(),
        event_id: "evt-metrics-park".to_string(),
        order_ref: None,
        subject: SubjectRef::tenant_device(tenant_id, "dev-fail"),
        email: None,
        phone: None,
        value: Some(1000),
        currency: Some("USD".to_string()),
        click_ids: ClickIds::default(),
        platforms: vec![PlatformKey::parse("meta").unwrap()],
        platforms_incomplete: false,
    };
    capture.execute(command_fail).await.unwrap();

    let worker_fail = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks,
        metrics.clone(),
    );

    // 4 failed attempts
    for _ in 1..=4 {
        worker_fail.execute(100).await.unwrap();
    }
    assert_eq!(
        metrics
            .conversion_upload_total()
            .with_label_values(&["meta", "failed"])
            .get(),
        4
    );
    assert_eq!(metrics.conversion_dlq_depth().get(), 0);

    // 5th attempt -> parked
    let report_park = worker_fail.execute(100).await.unwrap();
    assert_eq!(report_park.parked, 1);
    assert_eq!(
        metrics
            .conversion_upload_total()
            .with_label_values(&["meta", "parked"])
            .get(),
        1
    );
    assert_eq!(metrics.conversion_dlq_depth().get(), 1);
}

#[tokio::test]
async fn retraction_sweep_uploaded_withdrawn_event_retracts_while_consented_left_alone() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retract-withdrawn-vs-consented").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[0].clone(),
    ));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );

    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );

    // 1. Capture two events under different device subjects
    let command_withdrawn = TrackConversionCommand {
        tenant_id,
        event_name: "purchase".to_string(),
        event_id: "evt-retract-withdrawn".to_string(),
        order_ref: None,
        subject: SubjectRef::tenant_device(tenant_id, "dev-withdrawn-1"),
        email: None,
        phone: None,
        value: Some(1000),
        currency: Some("USD".to_string()),
        click_ids: ClickIds::default(),
        platforms: vec![PlatformKey::parse("google_ads").unwrap()],
        platforms_incomplete: false,
    };
    capture.execute(command_withdrawn).await.unwrap();

    let command_consented = TrackConversionCommand {
        tenant_id,
        event_name: "purchase".to_string(),
        event_id: "evt-retract-consented".to_string(),
        order_ref: None,
        subject: SubjectRef::tenant_device(tenant_id, "dev-consented-2"),
        email: None,
        phone: None,
        value: Some(2000),
        currency: Some("USD".to_string()),
        click_ids: ClickIds::default(),
        platforms: vec![PlatformKey::parse("google_ads").unwrap()],
        platforms_incomplete: false,
    };
    capture.execute(command_consented).await.unwrap();

    // 2. Upload both events
    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let upload_worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks.clone(),
        Arc::new(AdvertisingMetrics::default()),
    );
    let upload_report = upload_worker.execute(100).await.unwrap();
    assert_eq!(upload_report.uploaded, 2);

    let row1 = fetch_row(&pool, tenant_id, "evt-retract-withdrawn").await;
    assert_eq!(row1["upload_states"]["google_ads"]["status"], "uploaded");
    let row2 = fetch_row(&pool, tenant_id, "evt-retract-consented").await;
    assert_eq!(row2["upload_states"]["google_ads"]["status"], "uploaded");

    // 3. Directive resolver that denies ads_measurement for dev-withdrawn-1 only
    struct SubjectResolver {
        denied_device: String,
    }

    #[async_trait]
    impl DirectiveResolver for SubjectResolver {
        async fn resolve(
            &self,
            subject: &SubjectRef,
            purpose: ProcessingPurpose,
        ) -> Result<PurposeDirective, PrivacyError> {
            let is_denied = subject
                .identifiers
                .iter()
                .any(|identifier| identifier.value == self.denied_device);
            let state = if is_denied {
                DirectiveState::Denied
            } else {
                DirectiveState::Allowed
            };
            Ok(PurposeDirective {
                purpose,
                state,
                source: DirectiveSource::OptOut,
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

    let resolver = Arc::new(SubjectResolver {
        denied_device: "dev-withdrawn-1".to_string(),
    });

    let metrics = Arc::new(AdvertisingMetrics::default());
    let retract_worker = RetractWithdrawnConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        resolver,
        sinks,
        metrics.clone(),
    );

    let report = retract_worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 2);
    assert_eq!(report.retracted, 1);
    assert_eq!(report.unpropagated, 0);
    assert_eq!(report.failed, 0);

    // Assert sink received retract call only for the withdrawn event
    let retracted_events = google_sink.retracted_events();
    assert_eq!(retracted_events.len(), 1);
    assert_eq!(retracted_events[0].event_id, "evt-retract-withdrawn");

    // Check DB states: withdrawn -> retracted, consented -> uploaded
    let row_withdrawn_after = fetch_row(&pool, tenant_id, "evt-retract-withdrawn").await;
    assert_eq!(
        row_withdrawn_after["upload_states"]["google_ads"]["status"],
        "retracted"
    );

    let row_consented_after = fetch_row(&pool, tenant_id, "evt-retract-consented").await;
    assert_eq!(
        row_consented_after["upload_states"]["google_ads"]["status"],
        "uploaded"
    );
}

#[tokio::test]
async fn retraction_sweep_unsupported_adapter_transitions_to_explicit_unpropagated_state() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retract-unsupported-explicit").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[0].clone(),
    ));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );

    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-retract-unsupported");
    capture.execute(command).await.unwrap();

    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let upload_worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks.clone(),
        Arc::new(AdvertisingMetrics::default()),
    );
    upload_worker.execute(100).await.unwrap();

    let row_before = fetch_row(&pool, tenant_id, "evt-retract-unsupported").await;
    assert_eq!(
        row_before["upload_states"]["google_ads"]["status"],
        "uploaded"
    );

    // Configure sink to return an explicit unsupported error on retract
    google_sink.set_retract_error(Some(AdvertisingError::PlatformUnavailable(
        "conversion retraction is not supported by this platform".to_string(),
    )));

    let withdrawn_resolver = Arc::new(FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::AdsMeasurement,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    ));

    let registry = prometheus::Registry::new();
    let metrics = Arc::new(AdvertisingMetrics::new(&registry));
    let retract_worker = RetractWithdrawnConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        withdrawn_resolver,
        sinks,
        metrics.clone(),
    );

    let report = retract_worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.retracted, 0);
    assert_eq!(report.unpropagated, 1);
    assert_eq!(report.failed, 0);

    // Verify DB state is explicitly unpropagated with the reason recorded
    let row_after = fetch_row(&pool, tenant_id, "evt-retract-unsupported").await;
    assert_eq!(
        row_after["upload_states"]["google_ads"]["status"],
        "unpropagated"
    );
    let reason = row_after["upload_states"]["google_ads"]["reason"]
        .as_str()
        .unwrap();
    assert!(reason.contains("not supported"));

    // Verify metric recorded as unpropagated
    assert_eq!(
        metrics
            .conversion_retraction_total()
            .with_label_values(&["google_ads", "unpropagated"])
            .get(),
        1
    );
}

#[tokio::test]
async fn retraction_sweep_no_uploaded_platforms_is_zero_cost_noop() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retract-zero-cost-noop").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[0].clone(),
    ));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );

    // Event 1: Suppressed at capture (empty upload_states)
    let denied_resolver = FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::AdsMeasurement,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    );
    let capture_denied = capture_with(tx.clone(), denied_resolver, None, VendorRole::ThirdParty);
    capture_denied
        .execute(command_with_device_subject(
            tenant_id,
            "evt-noop-suppressed",
        ))
        .await
        .unwrap();

    // Event 2: Pending upload (not yet uploaded)
    let capture_allowed = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    capture_allowed
        .execute(command_with_device_subject(tenant_id, "evt-noop-pending"))
        .await
        .unwrap();

    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let retract_worker = RetractWithdrawnConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    let report = retract_worker.execute(100).await.unwrap();
    assert_eq!(report.checked, 0);
    assert_eq!(report.retracted, 0);
    assert_eq!(report.unpropagated, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(google_sink.retracted_events().len(), 0);
}

#[tokio::test]
async fn retraction_sweep_observability_metrics_and_transient_failure() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "retract-metrics-transient").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sink = Arc::new(FakeConversionSink::new(
        FakeAdPlatform::fixtures()[0].clone(),
    ));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sink.clone(),
    );

    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-retract-transient");
    capture.execute(command).await.unwrap();

    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    let upload_worker = UploadPendingConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks.clone(),
        Arc::new(AdvertisingMetrics::default()),
    );
    upload_worker.execute(100).await.unwrap();

    // Set transient 503 error on retract
    google_sink.set_retract_error(Some(AdvertisingError::PlatformUnavailable(
        "503 Service Unavailable".to_string(),
    )));

    let withdrawn_resolver = Arc::new(FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::AdsMeasurement,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    ));

    let registry = prometheus::Registry::new();
    let metrics = Arc::new(AdvertisingMetrics::new(&registry));
    let retract_worker = RetractWithdrawnConversions::new(
        tx.clone(),
        repo.clone(),
        connections.clone(),
        withdrawn_resolver,
        sinks.clone(),
        metrics.clone(),
    );

    // First attempt: transient failure, leaves state as uploaded for retry
    let report1 = retract_worker.execute(100).await.unwrap();
    assert_eq!(report1.checked, 1);
    assert_eq!(report1.failed, 1);
    assert_eq!(report1.retracted, 0);
    assert_eq!(report1.unpropagated, 0);
    assert_eq!(
        metrics
            .conversion_retraction_total()
            .with_label_values(&["google_ads", "failed"])
            .get(),
        1
    );

    let row_transient = fetch_row(&pool, tenant_id, "evt-retract-transient").await;
    assert_eq!(
        row_transient["upload_states"]["google_ads"]["status"],
        "uploaded"
    );

    // Fix error: next pass succeeds
    google_sink.set_retract_error(None);
    let report2 = retract_worker.execute(100).await.unwrap();
    assert_eq!(report2.checked, 1);
    assert_eq!(report2.retracted, 1);
    assert_eq!(report2.failed, 0);
    assert_eq!(report2.unpropagated, 0);
    assert_eq!(
        metrics
            .conversion_retraction_total()
            .with_label_values(&["google_ads", "retracted"])
            .get(),
        1
    );

    let row_retracted = fetch_row(&pool, tenant_id, "evt-retract-transient").await;
    assert_eq!(
        row_retracted["upload_states"]["google_ads"]["status"],
        "retracted"
    );
}

async fn fetch_audience_row(
    pool: &sanvi_persistence::PgPool,
    tenant_id: TenantId,
    audience_id: AudienceId,
) -> serde_json::Value {
    use sqlx::Row;
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let row = sqlx::query(
        "SELECT id, tenant_id, platform, name, status, failure_reason, external_ref,
                included_identifiers, last_synced_at
         FROM advertising.audiences WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(audience_id.as_i64())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    serde_json::json!({
        "id": row.get::<i64, _>("id").to_string(),
        "tenant_id": row.get::<i64, _>("tenant_id").to_string(),
        "platform": row.get::<String, _>("platform"),
        "name": row.get::<String, _>("name"),
        "status": row.get::<String, _>("status"),
        "failure_reason": row.get::<Option<String>, _>("failure_reason"),
        "external_ref": row.get::<Option<String>, _>("external_ref"),
        "included_identifiers": row.get::<serde_json::Value, _>("included_identifiers"),
        "last_synced_at": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_synced_at"),
    })
}

struct TestPermissionGate {
    allowed: bool,
}

impl sanvi_http::PermissionGate for TestPermissionGate {
    fn check<'a>(
        &'a self,
        _subject: &'a sanvi_http::Subject,
        _permission: &'a str,
        _resource: &'a sanvi_http::ResourceRef,
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

struct TestFeatureGate {
    enabled: bool,
}

impl sanvi_http::FeatureGate for TestFeatureGate {
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

#[tokio::test]
async fn audience_sync_eligible_subject_included_on_first_sync() {
    use sanvi_advertising::domain::AudienceRepository;

    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "aud-sync-first").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sync = Arc::new(FakeAudienceSync::new(FakeAdPlatform::fixtures()[0].clone()));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sync.clone(),
    );

    // Capture an event for a device subject with allowed directives
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-aud-eligible");
    capture.execute(command).await.unwrap();

    let audience_repo: Arc<dyn AudienceRepository> = Arc::new(SqlxCampaignRepository::new());
    let conversion_repo: Arc<dyn ConversionEventRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let ids = Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids()));

    let create_audience = CreateAudience::new(tx.clone(), audience_repo.clone(), ids);
    let audience = create_audience
        .execute(
            tenant_id,
            "High Value Customers".to_string(),
            PlatformKey::parse("google_ads").unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(audience.status, AudienceStatus::Building);
    assert_eq!(audience.included_identifiers.len(), 0);

    let sweep = SyncAudiences::new(
        tx.clone(),
        audience_repo.clone(),
        conversion_repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    let report = sweep.execute(3600, 100).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.synced, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(report.removed_identifiers, 0);

    let calls = google_sync.sync_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].removed.len(), 0);
    assert_eq!(calls[0].included.len(), 2); // email + phone

    let row = fetch_audience_row(&pool, tenant_id, audience.id).await;
    assert_eq!(row["status"], "active");
    assert!(row["external_ref"].is_string());
    assert!(row["last_synced_at"].is_string());
    assert_eq!(row["included_identifiers"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn audience_sync_opted_out_subject_is_removed_on_next_sync_and_absent_from_included() {
    use sanvi_advertising::domain::AudienceRepository;

    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "aud-sync-optout").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sync = Arc::new(FakeAudienceSync::new(FakeAdPlatform::fixtures()[0].clone()));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sync.clone(),
    );

    // 1. Capture event with allowed directives
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-aud-optout");
    capture.execute(command).await.unwrap();

    let audience_repo: Arc<dyn AudienceRepository> = Arc::new(SqlxCampaignRepository::new());
    let conversion_repo: Arc<dyn ConversionEventRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let ids = Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids()));

    let create_audience = CreateAudience::new(tx.clone(), audience_repo.clone(), ids);
    let audience = create_audience
        .execute(
            tenant_id,
            "Target Audience".to_string(),
            PlatformKey::parse("google_ads").unwrap(),
        )
        .await
        .unwrap();

    // First sync: member is included
    let sweep_initial = SyncAudiences::new(
        tx.clone(),
        audience_repo.clone(),
        conversion_repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks.clone(),
        Arc::new(AdvertisingMetrics::default()),
    );
    let report1 = sweep_initial.execute(3600, 100).await.unwrap();
    assert_eq!(report1.synced, 1);
    assert_eq!(google_sync.sync_calls().len(), 1);
    assert_eq!(google_sync.sync_calls()[0].included.len(), 2);

    // 2. Subject withdraws consent (opts out of TargetedAdvertising)
    let opt_out_resolver = Arc::new(FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::TargetedAdvertising,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    ));

    let sweep_refresh = SyncAudiences::new(
        tx.clone(),
        audience_repo.clone(),
        conversion_repo.clone(),
        connections.clone(),
        opt_out_resolver,
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    // Run sync with stale_after_secs = 0 so it runs immediately
    let report2 = sweep_refresh.execute(0, 100).await.unwrap();
    assert_eq!(report2.checked, 1);
    assert_eq!(report2.synced, 1);
    assert_eq!(report2.removed_identifiers, 2);

    let calls = google_sync.sync_calls();
    assert_eq!(calls.len(), 2);
    let second_call = &calls[1];
    assert_eq!(second_call.included.len(), 0); // absent from newly built list
    assert_eq!(second_call.removed.len(), 2); // present in removed list

    let row = fetch_audience_row(&pool, tenant_id, audience.id).await;
    assert_eq!(row["status"], "active");
    assert_eq!(row["included_identifiers"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn audience_sync_no_active_connection_is_skipped_not_failed() {
    use sanvi_advertising::domain::AudienceRepository;

    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "aud-sync-no-conn").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    // Do NOT seed connection

    let google_sync = Arc::new(FakeAudienceSync::new(FakeAdPlatform::fixtures()[0].clone()));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sync.clone(),
    );

    let audience_repo: Arc<dyn AudienceRepository> = Arc::new(SqlxCampaignRepository::new());
    let conversion_repo: Arc<dyn ConversionEventRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let ids = Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids()));

    let create_audience = CreateAudience::new(tx.clone(), audience_repo.clone(), ids);
    let audience = create_audience
        .execute(
            tenant_id,
            "Skipped Audience".to_string(),
            PlatformKey::parse("google_ads").unwrap(),
        )
        .await
        .unwrap();

    let sweep = SyncAudiences::new(
        tx.clone(),
        audience_repo.clone(),
        conversion_repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    let report = sweep.execute(0, 100).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.synced, 0);
    assert_eq!(report.failed, 0);

    // Sync port never called
    assert_eq!(google_sync.sync_calls().len(), 0);

    // DB state remains building and unsynced
    let row = fetch_audience_row(&pool, tenant_id, audience.id).await;
    assert_eq!(row["status"], "building");
    assert!(row["last_synced_at"].is_null());
}

#[tokio::test]
async fn audience_sync_unsupported_removal_marks_status_failed() {
    use sanvi_advertising::domain::AudienceRepository;
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "aud-sync-unsupported").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    let google_sync = Arc::new(FakeAudienceSync::new(FakeAdPlatform::fixtures()[0].clone()));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse("google_ads").unwrap(),
        google_sync.clone(),
    );

    // 1. Capture event with allowed directives
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-aud-unsup");
    capture.execute(command).await.unwrap();

    let audience_repo: Arc<dyn AudienceRepository> = Arc::new(SqlxCampaignRepository::new());
    let conversion_repo: Arc<dyn ConversionEventRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let ids = Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids()));

    let create_audience = CreateAudience::new(tx.clone(), audience_repo.clone(), ids);
    let audience = create_audience
        .execute(
            tenant_id,
            "Unsupported Removal Test".to_string(),
            PlatformKey::parse("google_ads").unwrap(),
        )
        .await
        .unwrap();

    // First sync
    let sweep_initial = SyncAudiences::new(
        tx.clone(),
        audience_repo.clone(),
        conversion_repo.clone(),
        connections.clone(),
        Arc::new(FakeDirectiveResolver::allow_all()),
        sinks.clone(),
        Arc::new(AdvertisingMetrics::default()),
    );
    sweep_initial.execute(3600, 100).await.unwrap();

    // 2. Opt out + configure sync port with unsupported removal
    google_sync.set_unsupported_removal(true);

    let opt_out_resolver = Arc::new(FakeDirectiveResolver::allow_all().with(
        ProcessingPurpose::SaleOrShare,
        DirectiveState::Denied,
        DirectiveSource::OptOut,
    ));

    let sweep_refresh = SyncAudiences::new(
        tx.clone(),
        audience_repo.clone(),
        conversion_repo.clone(),
        connections.clone(),
        opt_out_resolver,
        sinks,
        Arc::new(AdvertisingMetrics::default()),
    );

    let report = sweep_refresh.execute(0, 100).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.synced, 0);
    assert_eq!(report.unpropagated_removals, 1);

    let row = fetch_audience_row(&pool, tenant_id, audience.id).await;
    assert_eq!(row["status"], "failed");
    assert!(
        row["failure_reason"]
            .as_str()
            .unwrap()
            .contains("capability_unsupported")
    );
    assert!(row["last_synced_at"].is_string());
}

#[tokio::test]
async fn audience_http_create_refresh_list_round_trip() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "aud-http-roundtrip").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    seed_test_connection(&tx, &connections, tenant_id, "google_ads").await;

    // Capture an event
    let capture = capture_with(
        tx.clone(),
        FakeDirectiveResolver::allow_all(),
        None,
        VendorRole::ThirdParty,
    );
    let command = command_with_device_subject(tenant_id, "evt-aud-http");
    capture.execute(command).await.unwrap();

    let secret = "test-signing-secret-of-at-least-32-bytes";
    let state = create_test_advertising_state(pool.clone(), secret, 50, 50);

    let subject = sanvi_http::Subject {
        user_id: sanvi_kernel::UserId::from_snowflake(
            sanvi_testsupport::test_ids().next_id().expect("test id"),
        ),
        session_id: "sess_test".to_string(),
        aal: "aal2".to_string(),
        methods: vec!["password".to_string()],
        tenant_id: Some(tenant_id),
        role_ids: vec![],
        authenticated_at: Some(Utc::now()),
    };
    let tenant_context = sanvi_http::TenantContext {
        tenant_id,
        slug: "aud-http-roundtrip".to_string(),
        display_name: "Audience HTTP Test".to_string(),
        status: sanvi_http::tenancy::TenantRuntimeStatus::Active,
        region: "jp".to_string(),
        default_locale: "en".to_string(),
        resolution_source: sanvi_http::tenancy::ResolutionSource::Subdomain,
    };
    let permission_gate: Arc<dyn sanvi_http::PermissionGate> =
        Arc::new(TestPermissionGate { allowed: true });
    let feature_gate: Arc<dyn sanvi_http::FeatureGate> =
        Arc::new(TestFeatureGate { enabled: true });

    let app = test::init_service(
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
            .service(
                web::scope("/api/v1/tenant")
                    .configure(sanvi_advertising::routes::tracking_tenant_routes),
            ),
    )
    .await;

    // 1. Create audience
    let create_req = test::TestRequest::post()
        .uri("/api/v1/tenant/ads/audiences")
        .set_json(serde_json::json!({
            "name": "VIP Audience",
            "platform": "google_ads"
        }))
        .to_request();
    let create_res = test::call_service(&app, create_req).await;
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let create_body: serde_json::Value = test::read_body_json(create_res).await;
    let aud_id = create_body["id"].as_str().unwrap().to_string();
    assert_eq!(create_body["name"], "VIP Audience");
    assert_eq!(create_body["status"]["status"], "building");

    // 2. Refresh audience
    let refresh_req = test::TestRequest::post()
        .uri(&format!("/api/v1/tenant/ads/audiences/{aud_id}/refresh"))
        .to_request();
    let refresh_res = test::call_service(&app, refresh_req).await;
    assert_eq!(refresh_res.status(), StatusCode::OK);
    let refresh_body: serde_json::Value = test::read_body_json(refresh_res).await;
    assert_eq!(refresh_body["status"]["status"], "active");
    assert!(refresh_body["external_ref"].is_string());
    assert!(refresh_body["last_synced_at"].is_string());

    // 3. List audiences
    let list_req = test::TestRequest::get()
        .uri("/api/v1/tenant/ads/audiences")
        .to_request();
    let list_res = test::call_service(&app, list_req).await;
    assert_eq!(list_res.status(), StatusCode::OK);
    let list_body: Vec<serde_json::Value> = test::read_body_json(list_res).await;
    assert_eq!(list_body.len(), 1);
    assert_eq!(list_body[0]["id"], aud_id);
    assert_eq!(list_body[0]["status"]["status"], "active");
}
