//! Dependency-matrix injection tests: Google Ads (TASK-023, FR-1112/FR-1113).
//!
//! The invariant under injection: **an outage parks conversions with their
//! directive snapshot — never drops them, never uploads without it.** The
//! sweep counts *every* sink error as an attempt (five attempts, then the
//! conversion DLQ), so slow, erroring and down all converge on the parked
//! proof below; the transport probe pins that the real client surfaces
//! those modes as the retryable `PlatformUnavailable` the sweep counts.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::ads_harness;
use sanvi_advertising::adapters::GoogleConversionSink;
use sanvi_advertising::domain::{
    AdvertisingError, ConnectionAccessTokenProvider, ConnectionHandle, ConsentSnapshot,
    ConversionEvent, ConversionId, ConversionSinkPort,
};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

struct StaticTokens;

#[async_trait::async_trait]
impl ConnectionAccessTokenProvider for StaticTokens {
    async fn access_token(
        &self,
        _connection: &ConnectionHandle,
    ) -> Result<String, AdvertisingError> {
        Ok("matrix-token".to_string())
    }
}

fn probe_event(tenant_id: TenantId) -> ConversionEvent {
    ConversionEvent {
        id: ConversionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        event_id: "evt-probe".to_string(),
        dedupe_key: "probe-dedupe".to_string(),
        name: "purchase".to_string(),
        occurred_at: chrono::Utc::now(),
        value: None,
        value_source: None,
        order_ref: None,
        click_ids: Default::default(),
        hashed_identifiers: BTreeMap::new(),
        consent: ConsentSnapshot {
            resolver_version: "matrix".to_string(),
            jurisdiction: "eu".to_string(),
            purposes_asked: vec!["ads".to_string()],
            answers: BTreeMap::new(),
            signal_source: "matrix".to_string(),
        },
        upload_states: BTreeMap::new(),
        subject_key: sanvi_advertising::domain::SubjectKeyRef::Contact,
    }
}

fn probe_handle(tenant_id: TenantId) -> ConnectionHandle {
    ConnectionHandle {
        id: "conn-1".to_string(),
        tenant_id,
        external_account_id: "123-456-7890".to_string(),
    }
}

fn test_config() -> sanvi_advertising::AdvertisingConfig {
    sanvi_advertising::AdvertisingConfig {
        google_ads_developer_token: sanvi_telemetry::Redacted::new("matrix-dev-token".to_string()),
        ..Default::default()
    }
}

/// Google Ads · down/erroring/slow (transport) — the real sink surfaces
/// every outage mode as `PlatformUnavailable`, the retryable class the
/// upload sweep's attempt accounting counts.
#[tokio::test]
async fn google_ads_transport_outage_surfaces_a_retryable_error() {
    let tenant_id = TenantId::try_from(1_000_001_i64).unwrap();
    let tokens = Arc::new(StaticTokens);

    // Down: nothing listens.
    let (dead_base, _port) = common::closed_endpoint();
    let sink =
        GoogleConversionSink::with_custom_endpoints(&test_config(), dead_base, tokens.clone())
            .expect("sink builds");
    let err = sink
        .upload(&probe_handle(tenant_id), &probe_event(tenant_id))
        .await
        .expect_err("a dead Google Ads cannot upload");
    assert!(
        matches!(err, AdvertisingError::PlatformUnavailable(_)),
        "down must surface PlatformUnavailable, got: {err}"
    );

    // Erroring: every call 500s.
    let erroring = common::FaultServer::start(common::FaultMode::Status(500));
    let sink = GoogleConversionSink::with_custom_endpoints(
        &test_config(),
        erroring.base_url().to_string(),
        tokens.clone(),
    )
    .expect("sink builds");
    let err = sink
        .upload(&probe_handle(tenant_id), &probe_event(tenant_id))
        .await
        .expect_err("a 500ing Google Ads cannot upload");
    assert!(
        matches!(err, AdvertisingError::PlatformUnavailable(_)),
        "erroring must surface PlatformUnavailable, got: {err}"
    );

    // Slow: the answer arrives past the client deadline.
    let slow = common::FaultServer::start(common::FaultMode::Delay {
        delay_ms: 5_000,
        status: 200,
    });
    let sink = GoogleConversionSink::with_custom_endpoints_and_timeout(
        &test_config(),
        slow.base_url().to_string(),
        tokens,
        Duration::from_millis(300),
    )
    .expect("sink builds");
    let started = std::time::Instant::now();
    let err = sink
        .upload(&probe_handle(tenant_id), &probe_event(tenant_id))
        .await
        .expect_err("a too-slow Google Ads cannot upload");
    assert!(
        matches!(err, AdvertisingError::PlatformUnavailable(_)),
        "slow must surface PlatformUnavailable, got: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline must bound the wait, took {:?}",
        started.elapsed()
    );
}

/// Google Ads · outage — a failing Google platform parks the conversion
/// with its directive snapshot intact, never drops it, and never uploads
/// without the snapshot; the healthy platform is untouched.
#[tokio::test]
async fn google_ads_outage_parks_conversions_with_their_consent_snapshot() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = ads_harness::seed_tenant(&pool, "google-outage-parks").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let connections =
        Arc::new(sanvi_advertising::adapters::SqlxAdvertisingConnectionRepository::new());
    ads_harness::seed_connection(&tx, &connections, tenant_id, "google_ads").await;
    ads_harness::seed_connection(&tx, &connections, tenant_id, "meta").await;

    // Google fails (the outage), meta stays healthy (the control).
    let (sinks, _google, meta) = ads_harness::outage_sinks(
        "google_ads",
        ads_harness::GOOGLE_FIXTURE,
        "meta",
        ads_harness::META_FIXTURE,
    );

    ads_harness::capture(tx.clone())
        .execute(ads_harness::device_command(
            tenant_id,
            "evt-google-outage",
            &["google_ads", "meta"],
        ))
        .await
        .unwrap();

    let sweeper = ads_harness::sweeper(tx.clone(), sinks);

    // Five sweeps: google fails each time, then parks at the ceiling; meta
    // uploads on the first sweep and stays uploaded.
    for attempt in 1..=5 {
        let report = sweeper.execute(100).await.unwrap();
        assert_eq!(report.failed + report.parked, 1, "attempt {attempt}");
        assert!(report.uploaded <= 1, "attempt {attempt}");
    }

    let (consent, states) =
        ads_harness::fetch_consent_and_states(&pool, tenant_id, "evt-google-outage").await;
    assert_eq!(states["google_ads"]["status"], "parked");
    assert_eq!(states["meta"]["status"], "uploaded");

    // The parked conversion still carries its directive snapshot: the
    // later-justifiable upload is possible because the snapshot travelled
    // with the event, not because anyone re-asked.
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
    for (_, event) in meta.uploaded_records() {
        assert!(
            !event.consent.resolver_version.is_empty() && !event.consent.purposes_asked.is_empty(),
            "no upload may happen without its directive snapshot"
        );
    }

    // The event was never dropped: the row persists, ready for the
    // operator replay path (RetryConversion).
    assert_eq!(
        ads_harness::event_row_count(&pool, tenant_id, "evt-google-outage").await,
        1
    );
}
