//! First-request GPC recording and signed forwarding across the Advertising boundary.
use actix_web::{App, test, web};
use sanvi_advertising::adapters::remote::{
    AdvertisingAuthorizer, DELEGATION_HEADER, Delegation, GatewayState, RemoteClient,
    RequestBinding, ServiceConfig, TENANT_AUDIENCE,
};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use sanvi_privacy::adapters::{
    GpcSignalMiddleware, SqlxConsentRepository, SqlxDirectiveRepository,
    SqlxJurisdictionRepository, SqlxSignalRepository, SqlxSubjectRepository, SystemClock,
    SystemIdGenerator,
};
use sanvi_privacy::application::PersistedDirectiveResolver;
use sanvi_testsupport::test_ids;
use std::sync::{Arc, Mutex};
const ORIGIN: &str = "https://shop.acme-custom.example";
const SECRET: &str = "advertising-gateway-secret-01234567890123456789";
const BODY: &[u8] = br#"{"event_name":"purchase","event_id":"gpc-first"}"#;
struct NoTenantAccess;
#[async_trait::async_trait]
impl AdvertisingAuthorizer for NoTenantAccess {
    async fn check(
        &self,
        _: sanvi_kernel::UserId,
        _: Option<TenantId>,
        _: &str,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
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

#[actix_web::test]
async fn gpc_on_storefront_is_recorded_and_forwarded_on_first_beacon() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool().clone();
    let tenant_id = seed_tenant(&pool).await;
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

    let received = Arc::new(Mutex::new(None));
    let captured = received.clone();
    let base_url = super::support::spawn_stub(move |method, target, headers| {
        let captured = captured.clone();
        async move {
            let claims = Delegation::verify(
                SECRET,
                headers.get(DELEGATION_HEADER).unwrap(),
                TENANT_AUDIENCE,
                "public",
                &RequestBinding {
                    method: &method,
                    target: &target,
                    body: BODY,
                    content_type: "application/json",
                    if_match: "",
                },
            )
            .unwrap();
            *captured.lock().unwrap() = Some(claims);
            (202, "application/json", b"{}".to_vec())
        }
    })
    .await;
    let state = GatewayState {
        remote: RemoteClient::new(&ServiceConfig {
            base_url,
            signing_secret: sanvi_telemetry::Redacted::new(SECRET.into()),
            inbound_secret: sanvi_telemetry::Redacted::new(
                "advertising-inbound-secret-98765432109876543210".into(),
            ),
        })
        .unwrap(),
        authorizer: Arc::new(NoTenantAccess),
    };
    let app = test::init_service(
        App::new().app_data(web::Data::new(state)).service(
            web::scope("")
                .wrap(gpc)
                .configure(sanvi_advertising::routes::tracking_public_routes),
        ),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/v1/public/track")
            .peer_addr("192.0.2.1:41000".parse().unwrap())
            .insert_header(("origin", ORIGIN))
            .insert_header(("user-agent", "Mozilla/5.0"))
            .insert_header(("x-site-key", "signed-site-key"))
            .insert_header(("sec-gpc", "1"))
            .insert_header(("content-type", "application/json"))
            .set_payload(BODY)
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), actix_web::http::StatusCode::ACCEPTED);
    let claims = received
        .lock()
        .unwrap()
        .take()
        .expect("service received signed request");
    assert_eq!(claims.client_ip, Some("192.0.2.1".parse().unwrap()));
    assert_eq!(
        claims.forwarded_headers.get("origin").map(String::as_str),
        Some(ORIGIN)
    );
    assert_eq!(
        claims.forwarded_headers.get("sec-gpc").map(String::as_str),
        Some("1")
    );
    let device = claims
        .gpc_device
        .expect("new device included on first request");
    let snapshot = resolver
        .snapshot(&sanvi_privacy::domain::SubjectRef::tenant_device(
            tenant_id, &device,
        ))
        .await
        .unwrap();
    for purpose in [
        sanvi_privacy::ProcessingPurpose::SaleOrShare,
        sanvi_privacy::ProcessingPurpose::TargetedAdvertising,
    ] {
        let directive = snapshot
            .directives
            .iter()
            .find(|d| d.purpose == purpose)
            .unwrap();
        assert_eq!(directive.state, sanvi_privacy::DirectiveState::Denied);
        assert_eq!(
            directive.source,
            sanvi_privacy::domain::DirectiveSource::Signal
        );
    }
}
