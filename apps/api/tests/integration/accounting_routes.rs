//! Tenant accounting routes against the assembled app and a real Postgres.
//! Accounting runs in the standalone accounting-service; these tests verify
//! the platform gateway, HMAC delegation, security boundaries, internal delivery
//! receipts, and the remote privacy provider.

use std::collections::BTreeMap;
use std::sync::Arc;

use actix_web::test as aw_test;
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};

use sanvi_accounting::adapters::{
    RemoteAccountingPrivacyProvider,
    remote::{
        AUDIT_AUDIENCE, AuditCommand, DELEGATION_HEADER, Delegation, RequestBinding, ServiceConfig,
        TENANT_AUDIENCE,
    },
};
use sanvi_api::{
    AppConfig, IdentityDependencies, PlatformAdminDependencies, TenantCatalogAdapter,
    build_access_state, build_app, build_audit_state, build_identity_state,
    build_platform_admin_state, build_system_state, build_tenancy_state,
};
use sanvi_persistence::{PgPool, TxManager};
use sanvi_privacy::domain::{PersonalDataProvider, SubjectRef};
use sanvi_system::domain::BuildInfo;
use sanvi_telemetry::MetricsRegistry;

use super::support::{StubReply, spawn_stub};

// The 910_241_xxx range is owned by this suite.
const TENANT: i64 = 910_241_001;
const UNENTITLED_TENANT: i64 = 910_241_002;
const OWNER: i64 = 910_241_010;
const ADMIN: i64 = 910_241_011;
const MEMBER: i64 = 910_241_012;

const HOST: &str = "acct-a.itest";
const UNENTITLED_HOST: &str = "acct-b.itest";
const SESSION_COOKIE: &str = "ory_kratos_session";
const BOOKS: &str = "/api/v1/tenant/accounting/books";

/// A Kratos session: the token, the user it resolves to, and whether its
/// AAL2 authentication is inside the fresh-MFA window.
struct Persona {
    token: &'static str,
    user: i64,
    kratos_id: &'static str,
    fresh: bool,
}

const OWNER_FRESH: Persona = Persona {
    token: "acct-owner-fresh",
    user: OWNER,
    kratos_id: "91024100-0000-7000-8000-00000000a010",
    fresh: true,
};
const OWNER_STALE: Persona = Persona {
    token: "acct-owner-stale",
    user: OWNER,
    kratos_id: "91024100-0000-7000-8000-00000000a010",
    fresh: false,
};
const ADMIN_FRESH: Persona = Persona {
    token: "acct-admin-fresh",
    user: ADMIN,
    kratos_id: "91024100-0000-7000-8000-00000000a011",
    fresh: true,
};
const MEMBER_FRESH: Persona = Persona {
    token: "acct-member-fresh",
    user: MEMBER,
    kratos_id: "91024100-0000-7000-8000-00000000a012",
    fresh: true,
};
const PERSONAS: [&Persona; 4] = [&OWNER_FRESH, &OWNER_STALE, &ADMIN_FRESH, &MEMBER_FRESH];

fn email(user: i64) -> String {
    format!("user-{user}@acct.itest")
}

// ---------------------------------------------------------------------------
// Stubs and doubles
// ---------------------------------------------------------------------------

async fn kratos_stub(
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
) -> StubReply {
    if method != "GET" || target != "/sessions/whoami" {
        return (404, "application/json", Vec::new());
    }
    let cookie = headers
        .get("cookie")
        .map(String::as_str)
        .unwrap_or_default();
    let Some(persona) = PERSONAS
        .iter()
        .find(|persona| cookie.contains(&format!("{SESSION_COOKIE}={}", persona.token)))
    else {
        return (401, "application/json", Vec::new());
    };
    let authenticated_at = if persona.fresh {
        Utc::now()
    } else {
        Utc::now() - TimeDelta::hours(1)
    };
    let email = email(persona.user);
    let body = json!({
        "id": format!("session-{}", persona.token),
        "active": true,
        "authenticated_at": authenticated_at.to_rfc3339(),
        "identity": {
            "id": persona.kratos_id,
            "traits": {"email": email},
            "verifiable_addresses": [
                {"value": email, "verified": true, "status": "completed"}
            ]
        },
        "authentication_methods": [{"method": "password", "aal": "aal2"}]
    });
    (200, "application/json", body.to_string().into_bytes())
}

/// Confirms every role tuple: membership in Postgres is the deciding input.
async fn keto_stub(
    method: String,
    target: String,
    _headers: BTreeMap<String, String>,
) -> StubReply {
    if method != "GET" || !target.starts_with("/relation-tuples/check") {
        return (404, "application/json", Vec::new());
    }
    (200, "application/json", br#"{"allowed":true}"#.to_vec())
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Composition {
    Off,
    Remote,
    RemoteUnavailable,
}

fn config(kratos: String, keto: String, composition: Composition) -> AppConfig {
    let mut config = AppConfig::default();
    config.telemetry.service_name = "sanvi-accounting-routes".to_string();
    config.server.rate_limit.enabled = false;
    config.features.tenancy = true;
    config.features.identity = true;
    config.features.accounting = composition != Composition::Off;
    config.tenancy.base_domain = "itest".to_string();
    config.ory.kratos_public_url = kratos.clone();
    config.ory.kratos_admin_url = kratos;
    config.ory.keto_read_url = keto.clone();
    config.ory.keto_write_url = keto;
    config
}

async fn build(
    composition: Composition,
    pool: PgPool,
) -> actix_web::App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse<
            tracing_actix_web::StreamSpan<actix_web::body::BoxBody>,
        >,
        Error = actix_web::Error,
        InitError = (),
    > + 'static,
> {
    let kratos = spawn_stub(kratos_stub).await;
    let keto = spawn_stub(keto_stub).await;
    let mut config = config(kratos, keto, composition);
    if matches!(
        composition,
        Composition::Remote | Composition::RemoteUnavailable
    ) {
        const KEY: &str = "accounting-gateway-integration-key-0123456789";
        config.accounting_service.signing_secret = sanvi_telemetry::Redacted::new(KEY.into());
        config.accounting_service.inbound_secret =
            sanvi_telemetry::Redacted::new("accounting-gateway-delivery-key-9876543210".into());
        config.accounting_service.base_url = if composition == Composition::RemoteUnavailable {
            "http://127.0.0.1:1".into()
        } else {
            spawn_stub(|method, target, headers| async move {
                let token = headers.get("x-accounting-delegation").map(String::as_str).unwrap_or_default();
                let claims = Delegation::verify(KEY, token, TENANT_AUDIENCE, "tenant", &RequestBinding {
                    method: &method, target: &target, body: b"",
                    content_type: headers.get("content-type").map(String::as_str).unwrap_or_default(),
                    if_match: headers.get("if-match").map(String::as_str).unwrap_or_default(),
                });
                match claims {
                    Ok(claims) => (200, "application/json", serde_json::to_vec(&json!({
                        "tenant": claims.tenant.unwrap().tenant_id.to_string(),
                        "user": claims.subject.unwrap().user_id.to_string(), "permission": claims.permission,
                        "target": target, "cookie_forwarded": headers.contains_key("cookie"),
                        "authorization_forwarded": headers.contains_key("authorization"),
                    })).unwrap()),
                    Err(_) => (401, "application/json", b"{}".to_vec()),
                }
            }).await
        };
    }
    let metrics = MetricsRegistry::prometheus_only();
    let build_info = BuildInfo::new(
        "0.1.0-test".into(),
        "test".into(),
        "now".into(),
        "test".into(),
        "sanvi-accounting-routes".into(),
    );
    let system = build_system_state(build_info, "test", pool.clone(), None, 1);
    let ids = sanvi_testsupport::test_ids();
    let tenancy = build_tenancy_state(pool.clone(), ids.clone(), None, &config.tenancy, None);
    let role_catalog: Arc<dyn sanvi_identity::domain::RoleCatalog> =
        Arc::new(sanvi_access::adapters::SqlxRoleCatalog::new(
            TxManager::new(pool.clone(), ids.clone()),
            Arc::new(sanvi_access::adapters::SqlxRoleRepository::new()),
        ));
    let tenant_catalog: Arc<dyn sanvi_identity::domain::TenantCatalog> =
        Arc::new(TenantCatalogAdapter::new(
            TxManager::new(pool.clone(), ids.clone()),
            Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
        ));
    let identity = build_identity_state(
        pool.clone(),
        ids.clone(),
        None,
        &config.identity,
        &config.ory.kratos_public_url,
        &config.ory.kratos_admin_url,
        IdentityDependencies {
            role_catalog,
            tenant_catalog,
            notification_localizer: None,
            notification_gateway: None,
        },
    );
    let audit = build_audit_state(
        pool.clone(),
        ids.clone(),
        &config.audit,
        config.features.archival,
    );
    let access = build_access_state(
        pool.clone(),
        ids.clone(),
        &config.access,
        &config.ory.keto_read_url,
        &config.ory.keto_write_url,
        identity.memberships.clone(),
        audit.writer.clone(),
        Arc::new(sanvi_platform_admin::adapters::NoopApprovalGate),
        metrics.prometheus_registry(),
        None,
        Arc::new(sanvi_resilience::DegradationRegistry::default()),
    );
    let entitlement_admin: Arc<dyn sanvi_platform_admin::domain::EntitlementAdmin> =
        Arc::new(sanvi_api::AccessEntitlementAdmin::new(
            access.grant_entitlement.clone(),
            access.revoke_entitlement.clone(),
            access.list_tenant_overrides.clone(),
        ));
    let platform_admin = build_platform_admin_state(
        pool.clone(),
        ids.clone(),
        &config.platform_admin,
        PlatformAdminDependencies {
            tenants: Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
            memberships: identity.memberships.clone(),
            entitlements: Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new()),
            entitlement_admin,
            audit_writer: audit.writer.clone(),
            privacy_lock: Arc::new(sanvi_platform_admin::adapters::NoopPrivacyLock),
        },
    );

    build_app(
        config,
        metrics,
        system,
        tenancy,
        identity,
        access,
        audit,
        platform_admin,
        None, // billing
        None, // privacy
        None, // localization
        None, // theming
        None, // domains
        None, // advertising
        None, // redis
        None, // ssr_cache
        Arc::new(sanvi_resilience::DegradationRegistry::default()),
        None, // load_shed
    )
}

/// Tenants, users, the seeded system roles and their memberships, and the
/// `accounting.freee` entitlement for `TENANT` only.
async fn seed(pool: &PgPool) {
    let ids = sanvi_testsupport::test_ids();
    sanvi_access::application::SeedRoles::new(
        TxManager::new(pool.clone(), ids.clone()),
        Arc::new(sanvi_access::adapters::SqlxRoleRepository::new()),
        Arc::new(sanvi_access::adapters::SystemRoleIdGenerator::new(
            ids.clone(),
        )),
        Arc::new(sanvi_access::adapters::SystemClock),
    )
    .execute()
    .await
    .unwrap();

    let manager = TxManager::new(pool.clone(), ids.clone());
    let mut scope = manager
        .begin_unscoped("seed", "accounting-routes")
        .await
        .unwrap();
    let tx = scope.tx_mut().as_inner();
    for (id, slug) in [(TENANT, "acct-a"), (UNENTITLED_TENANT, "acct-b")] {
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
             VALUES ($1, $2, $2, 'active', 'us', 'en')",
        )
        .bind(id)
        .bind(slug)
        .execute(&mut **tx)
        .await
        .unwrap();
    }
    for persona in [&OWNER_FRESH, &ADMIN_FRESH, &MEMBER_FRESH] {
        sqlx::query(
            "INSERT INTO identity.users (id, kratos_identity_id, primary_email, email_verified, status)
             VALUES ($1, $2, $3, true, 'active')",
        )
        .bind(persona.user)
        .bind(uuid::Uuid::parse_str(persona.kratos_id).unwrap())
        .bind(email(persona.user))
        .execute(&mut **tx)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO access.features (key, name, kind, default_enabled)
         VALUES ('accounting.freee', 'freee accounting', 'boolean', false)
         ON CONFLICT (key) DO NOTHING",
    )
    .execute(&mut **tx)
    .await
    .unwrap();
    scope.commit().await.unwrap();

    let memberships = [
        (TENANT, OWNER, "tenant_owner"),
        (TENANT, ADMIN, "tenant_admin"),
        (TENANT, MEMBER, "tenant_member"),
        (UNENTITLED_TENANT, OWNER, "tenant_owner"),
    ];
    for (tenant, user, role) in memberships {
        let role = system_role(pool, role).await;
        let mut scope = manager.begin_for(tenant).await.unwrap();
        let tx = scope.tx_mut().as_inner();
        sqlx::query(
            "INSERT INTO identity.memberships (tenant_id, user_id, status) VALUES ($1, $2, 'active')",
        )
        .bind(tenant)
        .bind(user)
        .execute(&mut **tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO identity.membership_roles (tenant_id, user_id, role_id) VALUES ($1, $2, $3)",
        )
        .bind(tenant)
        .bind(user)
        .bind(role)
        .execute(&mut **tx)
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }

    let mut scope = manager.begin_for(TENANT).await.unwrap();
    sqlx::query(
        "INSERT INTO access.entitlements (tenant_id, feature_key, enabled, source)
         VALUES ($1, 'accounting.freee', true, 'override')",
    )
    .bind(TENANT)
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();
}

/// The ID of a seeded system role. Read outside any tenant scope: tenant
/// RLS hides the template roles, which carry no tenant ID.
async fn system_role(pool: &PgPool, key: &str) -> i64 {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager
        .begin_unscoped("seed", "accounting-routes")
        .await
        .unwrap();
    let id = sqlx::query_scalar(
        "SELECT id FROM access.roles WHERE key = $1 AND tenant_id IS NULL AND is_system",
    )
    .bind(key)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();
    id
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

struct Request<'a> {
    method: &'a str,
    uri: String,
    persona: &'a Persona,
    host: &'a str,
    body: Option<Value>,
}

impl<'a> Request<'a> {
    fn new(method: &'a str, uri: impl Into<String>) -> Self {
        Self {
            method,
            uri: uri.into(),
            persona: &OWNER_FRESH,
            host: HOST,
            body: None,
        }
    }
    fn persona(mut self, persona: &'a Persona) -> Self {
        self.persona = persona;
        self
    }
    fn host(mut self, host: &'a str) -> Self {
        self.host = host;
        self
    }

    async fn send<S, B>(self, app: &S) -> Reply
    where
        S: actix_web::dev::Service<
                actix_http::Request,
                Response = actix_web::dev::ServiceResponse<B>,
                Error = actix_web::Error,
            >,
        B: actix_web::body::MessageBody + 'static,
        B::Error: std::fmt::Debug,
    {
        let mut builder = aw_test::TestRequest::default()
            .method(actix_web::http::Method::from_bytes(self.method.as_bytes()).unwrap())
            .uri(&self.uri)
            .insert_header((actix_web::http::header::HOST, self.host))
            .cookie(actix_web::cookie::Cookie::new(
                SESSION_COOKIE,
                self.persona.token,
            ));
        if let Some(body) = self.body {
            builder = builder.set_json(body);
        }
        let response = aw_test::call_service(app, builder.to_request()).await;
        let status = response.status().as_u16();
        let bytes = actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap();
        Reply {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }
}

macro_rules! app {
    ($composition:expr) => {{
        let db = sanvi_testsupport::test_db().await;
        seed(&db.app_pool()).await;
        let app = aw_test::init_service(build($composition, db.app_pool()).await).await;
        (db, app)
    }};
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn routes_are_absent_when_the_flag_is_off() {
    let (_db, app) = app!(Composition::Off);
    let reply = Request::new("GET", BOOKS).send(&app).await;
    assert_eq!(reply.status, 404, "{}", reply.body);
}

#[actix_web::test]
async fn remote_gateway_preserves_platform_security_and_delegates_only_verified_identity() {
    let (_db, app) = app!(Composition::Remote);
    let reply = Request::new("GET", format!("{BOOKS}?limit=10"))
        .send(&app)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["tenant"], TENANT.to_string());
    assert_eq!(reply.json()["user"], OWNER.to_string());
    assert_eq!(reply.json()["permission"], "tenant.accounting.read");
    assert_eq!(reply.json()["target"], format!("{BOOKS}?limit=10"));
    assert_eq!(reply.json()["cookie_forwarded"], false);
    assert_eq!(reply.json()["authorization_forwarded"], false);
    let reply = Request::new("GET", BOOKS)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403);
    let reply = Request::new("GET", BOOKS)
        .host(UNENTITLED_HOST)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403);
    let reply = Request::new("POST", format!("{BOOKS}/1/connections/2/authorization"))
        .persona(&OWNER_STALE)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403);
}

#[actix_web::test]
async fn remote_outage_fails_closed_after_platform_checks() {
    let (_db, app) = app!(Composition::RemoteUnavailable);
    let reply = Request::new("GET", BOOKS).send(&app).await;
    assert_eq!(reply.status, 503);
    let reply = Request::new("GET", BOOKS)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403);
}

#[actix_web::test]
async fn remote_audit_and_event_retries_commit_one_platform_effect() {
    const SECRET: &str = "accounting-ingestion-secret-0123456789";
    let db = sanvi_testsupport::test_db().await;
    let ids = sanvi_testsupport::test_ids();
    let state = sanvi_api::accounting::AuditIngestState {
        secret: sanvi_telemetry::Redacted::new(SECRET.into()),
        transactions: TxManager::new(db.app_pool(), ids.clone()),
        writer: Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(ids)),
    };
    let app = aw_test::init_service(
        actix_web::App::new()
            .app_data(actix_web::web::Data::new(state))
            .route(
                "/internal/accounting/audit",
                actix_web::web::post().to(sanvi_api::accounting::ingest_audit),
            )
            .route(
                "/internal/accounting/events",
                actix_web::web::post().to(sanvi_api::accounting::ingest_event),
            ),
    )
    .await;
    let entry = sanvi_audit::AuditEntryData {
        occurred_at: Utc::now(),
        actor_type: sanvi_audit::ActorType::User,
        actor_id: Some(sanvi_kernel::UserId::try_from(OWNER).unwrap()),
        on_behalf_of: None,
        tenant_id: Some(sanvi_kernel::TenantId::try_from(TENANT).unwrap()),
        action: "accounting.test".into(),
        resource_type: Some("accounting_book".into()),
        resource_id: Some("123".into()),
        before: None,
        after: Some(json!({"revision":1})),
        request_id: Some("remote-request".into()),
        ip: None,
        user_agent: None,
    };
    for (path, purpose, body) in [
        (
            "/internal/accounting/audit",
            "audit",
            serde_json::to_vec(&AuditCommand {
                id: 919_100_001,
                entry: entry.clone(),
            })
            .unwrap(),
        ),
        (
            "/internal/accounting/events",
            "event",
            serde_json::to_vec(&sanvi_messaging::OutboxRecord {
                id: sanvi_kernel::EventId::try_from(919_000_001).unwrap(),
                aggregate: "accounting.book".into(),
                aggregate_id: sanvi_kernel::SnowflakeId::try_from(123).unwrap(),
                event_type: "accounting.created".into(),
                payload: json!({"book_id":"123"}),
                metadata: json!({}),
                occurred_at: Utc::now(),
                published_at: None,
                attempts: 0,
            })
            .unwrap(),
        ),
    ] {
        let claims = Delegation::new(
            AUDIT_AUDIENCE,
            purpose,
            &RequestBinding::json("POST", path, &body),
        );
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        for _ in 0..2 {
            let request = aw_test::TestRequest::post()
                .uri(path)
                .insert_header(("content-type", "application/json"))
                .insert_header((DELEGATION_HEADER, token.clone()))
                .set_payload(body.clone())
                .to_request();
            let response = aw_test::call_service(&app, request).await;
            let status = response.status().as_u16();
            let body = aw_test::read_body(response).await;
            assert_eq!(status, 204, "{path}: {}", String::from_utf8_lossy(&body));
        }
        let request = aw_test::TestRequest::post()
            .uri(path)
            .insert_header(("content-type", "application/json"))
            .insert_header((DELEGATION_HEADER, "invalid"))
            .set_payload(body)
            .to_request();
        assert_eq!(
            aw_test::call_service(&app, request).await.status().as_u16(),
            401
        );
    }
    // Receipts key on the command ID: an identical entry under a new ID is a new
    // audit record, and a reused ID with different content is refused.
    let send_audit = |command: AuditCommand| {
        let body = serde_json::to_vec(&command).unwrap();
        let claims = Delegation::new(
            AUDIT_AUDIENCE,
            "audit",
            &RequestBinding::json("POST", "/internal/accounting/audit", &body),
        );
        aw_test::TestRequest::post()
            .uri("/internal/accounting/audit")
            .insert_header(("content-type", "application/json"))
            .insert_header((
                DELEGATION_HEADER,
                sanvi_crypto::sign(SECRET.as_bytes(), &claims),
            ))
            .set_payload(body)
            .to_request()
    };
    let twin = send_audit(AuditCommand {
        id: 919_100_002,
        entry: entry.clone(),
    });
    assert_eq!(
        aw_test::call_service(&app, twin).await.status().as_u16(),
        204
    );
    let mut altered = entry;
    altered.request_id = Some("other-request".into());
    let reused = send_audit(AuditCommand {
        id: 919_100_001,
        entry: altered,
    });
    assert_eq!(
        aw_test::call_service(&app, reused).await.status().as_u16(),
        409
    );
    let admin = db.admin_pool().await;
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit.entries WHERE action = 'accounting.test'")
            .fetch_one(&admin)
            .await
            .unwrap();
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform.outbox WHERE id = 919000001")
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(audits, 2);
    assert_eq!(events, 1);
}

#[actix_web::test]
async fn remote_privacy_provider_calls_service_and_fails_closed() {
    const SIGNING_SECRET: &str = "accounting-privacy-signing-0123456789";
    const INBOUND_SECRET: &str = "accounting-privacy-inbound-9876543210";
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stub_url = spawn_stub(move |method, target, headers| {
        let tx = tx.clone();
        async move {
            let token = headers
                .get(DELEGATION_HEADER)
                .map(String::as_str)
                .unwrap_or_default();
            tx.send((method, target, token.to_string())).unwrap();
            (204, "application/json", Vec::new())
        }
    })
    .await;

    let config = ServiceConfig {
        base_url: stub_url,
        signing_secret: sanvi_telemetry::Redacted::new(SIGNING_SECRET.into()),
        inbound_secret: sanvi_telemetry::Redacted::new(INBOUND_SECRET.into()),
    };
    let provider = RemoteAccountingPrivacyProvider::new(&config).unwrap();
    let subject = SubjectRef {
        kind: sanvi_privacy::domain::SubjectKind::TenantUser,
        tenant_id: Some(sanvi_kernel::TenantId::try_from(TENANT).unwrap()),
        key: format!("user:{OWNER}"),
        identifiers: Vec::new(),
    };
    let bundle = provider.collect(&subject).await.unwrap();
    assert_eq!(bundle.context, "accounting");
    let (method, target, token) = rx.recv().await.unwrap();
    assert_eq!(method, "POST");
    assert_eq!(target, "/internal/privacy/review");
    assert!(!token.is_empty());

    let broken_config = ServiceConfig {
        base_url: "http://127.0.0.1:1".into(),
        signing_secret: sanvi_telemetry::Redacted::new(SIGNING_SECRET.into()),
        inbound_secret: sanvi_telemetry::Redacted::new(INBOUND_SECRET.into()),
    };
    let broken_provider = RemoteAccountingPrivacyProvider::new(&broken_config).unwrap();
    assert!(broken_provider.collect(&subject).await.is_err());
}
