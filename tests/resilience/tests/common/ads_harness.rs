//! Shared sweep harness for the ad-platform outage rows (TASK-023,
//! FR-1112/FR-1113): capture a conversion against the test database, then
//! sweep it with one platform's sink failing. The injection is a failing
//! port implementation — one of the shapes the dependency matrix allows —
//! and the sweep must park the conversion **with its directive snapshot**,
//! never drop it, and never upload without the snapshot.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use sanvi_advertising::adapters::{
    FakeAdPlatform, FakeConversionSink, SqlxAdvertisingConnectionRepository,
    SqlxCampaignRepository, SystemIdGenerator,
};
use sanvi_advertising::application::tracking::UploadPendingConversions;
use sanvi_advertising::application::tracking::capture::{
    CaptureConversion, TrackConversionCommand,
};
use sanvi_advertising::domain::{
    AdvertisingConnection, AdvertisingConnectionRepository, AdvertisingError, ClickIds,
    ConnectionId, ConversionEventRepository, ConversionSinkPort, PaymentLookup, PaymentLookupPort,
    PlatformKey, SecretRef, VendorRole, VendorRolePort,
};
use sanvi_kernel::TenantId;
use sanvi_persistence::{ScopedTx, TxManager};
use sanvi_privacy::domain::{
    DirectiveResolver, DirectiveSnapshot, DirectiveSource, DirectiveState, JurisdictionRef,
    PrivacyError, ProcessingPurpose, PurposeDirective, SubjectRef,
};

/// Fixture index of each platform in [`FakeAdPlatform::fixtures`]
/// (platform catalog order).
pub const GOOGLE_FIXTURE: usize = 0;
pub const META_FIXTURE: usize = 1;

pub async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> TenantId {
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

/// A directive resolver that allows everything — the outage rows prove the
/// snapshot *travels*, not the gate's verdicts.
pub struct AllowAllResolver;

#[async_trait]
impl DirectiveResolver for AllowAllResolver {
    async fn resolve(
        &self,
        _subject: &SubjectRef,
        purpose: ProcessingPurpose,
    ) -> Result<PurposeDirective, PrivacyError> {
        Ok(PurposeDirective {
            purpose,
            state: DirectiveState::Allowed,
            source: DirectiveSource::Default,
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

struct NoPaymentLookup;

#[async_trait]
impl PaymentLookupPort for NoPaymentLookup {
    async fn lookup_payment_value(
        &self,
        _scope: &mut ScopedTx<'_>,
        _tenant_id: TenantId,
        _order_ref: &str,
    ) -> Result<Option<PaymentLookup>, AdvertisingError> {
        Ok(None)
    }
}

struct ThirdPartyVendor;

#[async_trait]
impl VendorRolePort for ThirdPartyVendor {
    async fn lookup_role(&self, _platform: &PlatformKey) -> Result<VendorRole, AdvertisingError> {
        Ok(VendorRole::ThirdParty)
    }
}

pub fn capture(tx: TxManager) -> CaptureConversion {
    CaptureConversion::new(
        tx,
        Arc::new(SqlxCampaignRepository::new()),
        Arc::new(NoPaymentLookup),
        Arc::new(ThirdPartyVendor),
        Arc::new(AllowAllResolver),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    )
}

/// A device-identified capture across the named platforms: device subjects
/// carry no reconstructable key material, so the frozen snapshot is the
/// whole justification the later upload runs on.
pub fn device_command(
    tenant_id: TenantId,
    event_id: &str,
    platforms: &[&str],
) -> TrackConversionCommand {
    TrackConversionCommand {
        tenant_id,
        event_name: "purchase".to_string(),
        event_id: event_id.to_string(),
        order_ref: None,
        click_ids: ClickIds::default(),
        value: None,
        currency: None,
        email: None,
        phone: None,
        subject: SubjectRef::tenant_device(tenant_id, "device-1"),
        platforms: platforms
            .iter()
            .map(|platform| PlatformKey::parse(platform).unwrap())
            .collect(),
        platforms_incomplete: false,
    }
}

pub async fn seed_connection(
    tx: &TxManager,
    connections: &SqlxAdvertisingConnectionRepository,
    tenant_id: TenantId,
    platform: &str,
) {
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
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    connections.upsert(&mut scope, &connection).await.unwrap();
    scope.commit().await.unwrap();
}

/// One failing sink plus one healthy sink, keyed by platform: the outage
/// under test and the control that must keep uploading.
pub fn outage_sinks(
    failing_platform: &str,
    failing_fixture: usize,
    healthy_platform: &str,
    healthy_fixture: usize,
) -> (
    BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>>,
    Arc<FakeConversionSink>,
    Arc<FakeConversionSink>,
) {
    let fixtures = FakeAdPlatform::fixtures();
    let failing = Arc::new(
        FakeConversionSink::new(fixtures[failing_fixture].clone()).with_upload_error(
            AdvertisingError::PlatformUnavailable(format!("simulated {failing_platform} outage")),
        ),
    );
    let healthy = Arc::new(FakeConversionSink::new(fixtures[healthy_fixture].clone()));
    let mut sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> = BTreeMap::new();
    sinks.insert(
        PlatformKey::parse(failing_platform).unwrap(),
        failing.clone(),
    );
    sinks.insert(
        PlatformKey::parse(healthy_platform).unwrap(),
        healthy.clone(),
    );
    (sinks, failing, healthy)
}

pub fn sweeper(
    tx: TxManager,
    sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>>,
) -> UploadPendingConversions {
    let repo: Arc<dyn ConversionEventRepository> = Arc::new(SqlxCampaignRepository::new());
    UploadPendingConversions::new(
        tx.clone(),
        repo,
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        Arc::new(AllowAllResolver),
        sinks,
        Arc::new(sanvi_advertising::adapters::AdvertisingMetrics::default()),
    )
}

/// The conversion row's `upload_states` and `consent` columns: what the
/// sweep decided per platform, and the directive snapshot it decided with.
pub async fn fetch_consent_and_states(
    pool: &sanvi_persistence::PgPool,
    tenant_id: TenantId,
    event_id: &str,
) -> (serde_json::Value, serde_json::Value) {
    use sqlx::Row;
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let row = sqlx::query(
        "SELECT consent, upload_states FROM advertising.conversion_events
         WHERE tenant_id = $1 AND event_id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(event_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    (
        row.get::<serde_json::Value, _>("consent"),
        row.get::<serde_json::Value, _>("upload_states"),
    )
}

pub async fn event_row_count(
    pool: &sanvi_persistence::PgPool,
    tenant_id: TenantId,
    event_id: &str,
) -> i64 {
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
