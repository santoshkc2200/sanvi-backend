//! TASK-029 (11.10 Step 3): the GPC signal arriving on a storefront under
//! a tenant custom domain — the case most likely to be quietly broken,
//! because the signal arrives on the tenant's own domain, a code path
//! phase 05 never saw.
//!
//! The drill boots the real beacon route under the real GPC middleware,
//! sends the *first* request (no device cookie can exist yet) carrying
//! `Sec-GPC: 1`, and asserts both halves of the obligation:
//!
//! - the signal is recorded (the directive snapshot's evidence row), and
//! - tracking is suppressed *on that very first request*: the stored
//!   conversion event's frozen consent snapshot says `denied` with the
//!   `gpc` signal source, and nothing is queued for upload to any
//!   platform.

use actix_web::{
    App,
    body::BoxBody,
    dev::{ServiceFactory, ServiceRequest, ServiceResponse},
    http::StatusCode,
    test, web,
};
use sanvi_advertising::AdvertisingConfig;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use sanvi_privacy::adapters::{
    GpcSignalMiddleware, SqlxConsentRepository, SqlxDirectiveRepository,
    SqlxJurisdictionRepository, SqlxSignalRepository, SqlxSubjectRepository, SystemClock,
    SystemIdGenerator,
};
use sanvi_privacy::application::PersistedDirectiveResolver;
use sanvi_testsupport::test_ids;
use std::sync::Arc;

const ORIGIN: &str = "https://shop.acme-custom.example";

async fn drill_app(
    pool: sanvi_persistence::PgPool,
) -> (
    App<
        impl ServiceFactory<
            ServiceRequest,
            Config = (),
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
            InitError = (),
        >,
    >,
    actix_web::web::Data<sanvi_advertising::adapters::http::AdvertisingState>,
    std::sync::Arc<dyn sanvi_privacy::domain::DirectiveResolver>,
) {
    let ids = test_ids();
    let tx = TxManager::new(pool.clone(), ids.clone());
    let clock: std::sync::Arc<dyn sanvi_privacy::domain::Clock> = Arc::new(SystemClock);
    let subjects: std::sync::Arc<dyn sanvi_privacy::domain::SubjectRepository> =
        Arc::new(SqlxSubjectRepository::new());
    let signals: std::sync::Arc<dyn sanvi_privacy::domain::SignalRepository> =
        Arc::new(SqlxSignalRepository::new());
    let gpc = GpcSignalMiddleware::new(
        tx.clone(),
        signals.clone(),
        subjects.clone(),
        clock.clone(),
        Arc::new(SystemIdGenerator::new(ids.clone())),
        "sanvi_gpc_device".to_string(),
        30,
    );

    // The directive resolver the capture gate asks: the persisted one, over
    // the same ledger the middleware writes signals into.
    let resolver: std::sync::Arc<dyn sanvi_privacy::domain::DirectiveResolver> =
        Arc::new(PersistedDirectiveResolver::new(
            tx.clone(),
            subjects.clone(),
            Arc::new(SqlxJurisdictionRepository::new()),
            Arc::new(SqlxConsentRepository::new()),
            signals.clone(),
            Arc::new(SqlxDirectiveRepository::new()),
            clock.clone(),
        ));

    let adv_config = AdvertisingConfig {
        oauth_state_signing_secret: sanvi_telemetry::Redacted::new(
            "drill-oauth-state-signing-secret-0123456789ab".to_string(),
        ),
        tracking_site_key_signing_secret: sanvi_telemetry::Redacted::new(
            "drill-tracking-site-key-secret-0123456789ab".to_string(),
        ),
        // The per-tenant token envelope's KEK: base64 of 32 bytes, as the
        // config validation demands.
        token_kek: sanvi_telemetry::Redacted::new(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            [9u8; 32],
        )),
        ..AdvertisingConfig::default()
    };
    let advertising = sanvi_api::build_advertising_state(
        pool.clone(),
        ids.clone(),
        &adv_config,
        resolver.clone(),
        Arc::new(sanvi_privacy::adapters::SqlxSubProcessorRepository::new()),
    )
    .await
    .expect("advertising state builds");

    // Exactly the storefront surface the signal arrives on: the public
    // beacon scope wrapped with the GPC middleware (the assembly
    // `build_app` performs when both contexts are wired). The state handle
    // rides along so a swallowed capture error (the beacon always answers
    // 202) can be surfaced by the drill itself.
    let state = web::Data::new(advertising);
    let app = App::new().app_data(state.clone()).service(
        web::scope("")
            .wrap(gpc)
            .configure(sanvi_advertising::routes::tracking_public_routes),
    );
    (app, state, resolver)
}

/// The conversion tables are tenant-RLS'd; a test read needs the tenant
/// GUC set or it sees nothing — the same view the API's tenant-scoped
/// callers have.
async fn tenant_scoped_read(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    sql: &str,
    event_id: &str,
) -> Vec<(serde_json::Value, serde_json::Value)> {
    let ids = test_ids();
    let tx = TxManager::new(pool.clone(), ids);
    let mut scope = tx
        .begin_for(tenant_id.as_i64())
        .await
        .expect("tenant scope");
    let rows: Vec<(serde_json::Value, serde_json::Value)> = sqlx::query_as(sqlx::AssertSqlSafe(
        format!("{sql} WHERE tenant_id = $1 AND event_id = $2"),
    ))
    .bind(tenant_id.as_i64())
    .bind(event_id)
    .fetch_all(&mut **scope.tx_mut().as_inner())
    .await
    .expect("event read");
    scope.commit().await.expect("commit");
    rows
}

async fn seed_tenant(pool: &sqlx::PgPool) -> TenantId {
    let tenant_id = TenantId::try_from(902_000_001_i64).expect("tenant id");
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region)
         VALUES ($1, 'gpc-drill', 'GPC Drill Shop', 'active', 'jp')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(tenant_id.as_i64())
    .execute(pool)
    .await
    .expect("seed tenant");
    tenant_id
}

#[tokio::test]
async fn gpc_on_the_storefront_suppresses_tracking_on_the_first_request() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool().clone();
    let tenant_id = seed_tenant(&pool).await;

    // The storefront's site key is minted for the custom-domain origin —
    // the beacon posts from the tenant's own domain.
    let site_key = sanvi_advertising::application::tracking::site_key::generate_site_key(
        "drill-tracking-site-key-secret-0123456789ab",
        tenant_id,
        ORIGIN,
    )
    .expect("site key");

    let (app, state, resolver) = drill_app(pool.clone()).await;
    let app = test::init_service(app).await;

    // The FIRST request: no device cookie exists yet for the handler to
    // read back — the middleware mints the device, records the signal, and
    // hands the handler the canonical key via the request extension.
    let event_id = format!("gpc-drill-{}", uuid::Uuid::now_v7());
    let request = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header((actix_web::http::header::ORIGIN, ORIGIN))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header(("Sec-GPC", "1"))
        .insert_header((
            actix_web::http::header::USER_AGENT,
            "Mozilla/5.0 (Macintosh) gpc-drill",
        ))
        .peer_addr("127.0.0.1:41000".parse().expect("addr"))
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": event_id,
            "value": 4200,
            "currency": "JPY",
        }))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "the beacon always answers 202; a different status means the drill hit a guard"
    );

    let set_cookie = response
        .headers()
        .get(actix_web::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // The table is tenant-RLS'd; the platform scope is the audited
    // ground-truth read (the same view the resolver itself uses).
    let signal_rows: Vec<(String, String)> = {
        let ids = test_ids();
        let tx = TxManager::new(pool.clone(), ids);
        let mut scope = tx
            .begin_unscoped("gpc_drill_signal_read", "test")
            .await
            .expect("platform scope");
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT device_ref, purposes::text FROM privacy.preference_signals
             WHERE signal = 'gpc'",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .expect("signal read");
        scope.commit().await.expect("commit");
        rows
    };

    // The suppression is visible on the stored event: the frozen consent
    // snapshot names the gpc signal, every asked purpose is denied, and no
    // platform upload was queued.
    let rows = tenant_scoped_read(
        &pool,
        tenant_id,
        "SELECT consent, upload_states FROM advertising.conversion_events",
        &event_id,
    )
    .await;
    if rows.is_empty() {
        // Surface the swallowed capture error: the beacon always answers
        // 202, so the drill re-runs the same pipeline the handler ran —
        // the real execute, insert included — and reports the failure it
        // hid.
        let dedup = tenant_scoped_read(
            &pool,
            tenant_id,
            "SELECT to_jsonb(event_id), to_jsonb(dedupe_key) FROM advertising.conversion_event_dedup",
            &event_id,
        )
        .await
        .len() as i64;
        let error = state
            .capture
            .execute(
                sanvi_advertising::application::tracking::capture::TrackConversionCommand {
                    tenant_id,
                    event_name: "purchase".to_string(),
                    event_id: format!("{event_id}-probe"),
                    order_ref: None,
                    click_ids: sanvi_advertising::domain::ClickIds::default(),
                    value: Some(4200),
                    currency: Some("JPY".to_string()),
                    email: None,
                    phone: None,
                    subject: sanvi_privacy::domain::SubjectRef::tenant_device(
                        tenant_id,
                        "probe-device",
                    ),
                    platforms: Vec::new(),
                    platforms_incomplete: true,
                },
            )
            .await;
        match error {
            Err(error) => panic!(
                "beacon captured nothing (dedup rows: {dedup}); the capture pipeline fails with: {error}"
            ),
            Ok(()) => panic!(
                "beacon captured nothing (dedup rows: {dedup}) but a direct capture probe succeeded — the handler's own guards ate the beacon"
            ),
        }
    }
    assert_eq!(rows.len(), 1, "the beacon was captured exactly once");
    let (consent, upload_state) = &rows[0];
    for purpose in ["ads_measurement", "sale_or_share", "targeted_advertising"] {
        assert_eq!(
            consent
                .get("answers")
                .and_then(|a| a.get(purpose))
                .and_then(|v| v.as_str()),
            Some("denied"),
            "{purpose} denied on the very first request: {consent}"
        );
    }
    assert_eq!(
        upload_state,
        &serde_json::json!({}),
        "a suppressed event reaches no platform: {upload_state}"
    );

    // The signal itself is on the record: a gpc signal for the minted
    // device (the one the cookie carries), carrying the opt-out purposes.
    assert!(!signal_rows.is_empty(), "the GPC signal was recorded");
    let cookie_device = set_cookie.as_deref().and_then(|cookie| {
        cookie
            .split(';')
            .next()
            .and_then(|pair| pair.split_once('='))
            .filter(|(name, _)| *name == "sanvi_gpc_device")
            .map(|(_, value)| value.to_string())
    });
    let cookie_device = cookie_device.expect("the middleware set the device cookie");
    assert!(
        signal_rows
            .iter()
            .any(|(device_ref, _)| *device_ref == cookie_device),
        "the signal's device is the one the middleware minted: {signal_rows:?} vs {cookie_device}"
    );
    assert!(
        signal_rows
            .iter()
            .any(|(_, purposes)| purposes.contains("sale_or_share")),
        "the signal carries sale/share and targeted-ads purposes: {signal_rows:?}"
    );

    // The directive snapshot for that very device: the recorded signal
    // governs it — sale/share and targeted ads are denied *by the signal*
    // (source `signal`), not by a jurisdiction default. This is the
    // unambiguous GPC provenance the event's frozen snapshot cannot show
    // where the jurisdiction default denies anyway.
    let device_subject =
        sanvi_privacy::domain::SubjectRef::tenant_device(tenant_id, &cookie_device);
    let snapshot = resolver
        .snapshot(&device_subject)
        .await
        .expect("device snapshot");
    for purpose in [
        sanvi_privacy::domain::ProcessingPurpose::SaleOrShare,
        sanvi_privacy::domain::ProcessingPurpose::TargetedAdvertising,
    ] {
        let directive = snapshot
            .directives
            .iter()
            .find(|d| d.purpose == purpose)
            .unwrap_or_else(|| panic!("directive for {:?}", purpose));
        assert_eq!(
            directive.state,
            sanvi_privacy::domain::DirectiveState::Denied,
            "{:?} denied for the GPC device",
            purpose
        );
        assert_eq!(
            directive.source,
            sanvi_privacy::domain::DirectiveSource::Signal,
            "the denial carries the signal's provenance: {:?}",
            directive
        );
    }

    // The negative control: the same storefront without the signal — a
    // different device, no Sec-GPC — has no signal governing it, so its
    // sale/share directive's provenance is never the signal. The
    // suppression above came from the recorded GPC signal, not the fixture
    // shape.
    let control_id = format!("gpc-control-{}", uuid::Uuid::now_v7());
    let control = test::TestRequest::post()
        .uri("/api/v1/public/track")
        .insert_header((actix_web::http::header::ORIGIN, ORIGIN))
        .insert_header(("X-Site-Key", site_key.as_str()))
        .insert_header((
            actix_web::http::header::USER_AGENT,
            "Mozilla/5.0 (Macintosh) gpc-drill",
        ))
        .peer_addr("127.0.0.1:41001".parse().expect("addr"))
        .set_json(serde_json::json!({
            "event_name": "purchase",
            "event_id": control_id,
            "device_id": "control-device",
        }))
        .to_request();
    let _ = test::call_service(&app, control).await;
    let control_rows = tenant_scoped_read(
        &pool,
        tenant_id,
        "SELECT consent, to_jsonb(event_id) FROM advertising.conversion_events",
        &control_id,
    )
    .await;
    assert_eq!(control_rows.len(), 1, "the control beacon was captured");
    let control_subject =
        sanvi_privacy::domain::SubjectRef::tenant_device(tenant_id, "control-device");
    let control_snapshot = resolver
        .snapshot(&control_subject)
        .await
        .expect("control snapshot");
    let control_sale = control_snapshot
        .directives
        .iter()
        .find(|d| d.purpose == sanvi_privacy::domain::ProcessingPurpose::SaleOrShare)
        .expect("control sale/share directive");
    assert_ne!(
        control_sale.source,
        sanvi_privacy::domain::DirectiveSource::Signal,
        "a device with no GPC signal is not governed by one: {:?}",
        control_sale
    );
}
