//! Tenant domains routes against the assembled app and a real Postgres.
//! Mirrors the precedent in accounting_routes.rs.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use actix_web::test as aw_test;
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};

use sanvi_api::{
    AppConfig, IdentityDependencies, PlatformAdminDependencies, TenantCatalogAdapter,
    build_access_state, build_app, build_audit_state, build_identity_state,
    build_platform_admin_state, build_system_state, build_tenancy_state,
};
use sanvi_domains::{
    adapters::{
        LiveDomainCache,
        remote::{
            AUDIT_AUDIENCE, AuditCommand, DELEGATION_HEADER, Delegation, RequestBinding,
            ServiceConfig, TENANT_AUDIENCE,
        },
    },
    application::LiveDomainLookupService,
};
use sanvi_persistence::{PgPool, TxManager};
use sanvi_system::domain::BuildInfo;
use sanvi_telemetry::MetricsRegistry;

use super::support::{StubReply, spawn_stub};

// The 910_242_xxx range is owned by this suite.
const TENANT: i64 = 910_242_001;
const UNENTITLED_TENANT: i64 = 910_242_002;
const SUSPENDED_TENANT: i64 = 910_242_003;
const OWNER: i64 = 910_242_010;
const ADMIN: i64 = 910_242_011;
const MEMBER: i64 = 910_242_012;

const HOST: &str = "domains-a.itest";
const UNENTITLED_HOST: &str = "domains-b.itest";
const SESSION_COOKIE: &str = "ory_kratos_session";

const DOMAINS: &str = "/api/v1/tenant/domains";
const SEARCH: &str = "/api/v1/tenant/domains/search";
const ORDERS: &str = "/api/v1/tenant/domains/orders";

struct Persona {
    token: &'static str,
    user: i64,
    kratos_id: &'static str,
    fresh: bool,
}

const OWNER_FRESH: Persona = Persona {
    token: "domains-owner-fresh",
    user: OWNER,
    kratos_id: "91024200-0000-7000-8000-00000000a010",
    fresh: true,
};
const OWNER_STALE: Persona = Persona {
    token: "domains-owner-stale",
    user: OWNER,
    kratos_id: "91024200-0000-7000-8000-00000000a010",
    fresh: false,
};
const ADMIN_FRESH: Persona = Persona {
    token: "domains-admin-fresh",
    user: ADMIN,
    kratos_id: "91024200-0000-7000-8000-00000000a011",
    fresh: true,
};
const MEMBER_FRESH: Persona = Persona {
    token: "domains-member-fresh",
    user: MEMBER,
    kratos_id: "91024200-0000-7000-8000-00000000a012",
    fresh: true,
};
const PERSONAS: [&Persona; 4] = [&OWNER_FRESH, &OWNER_STALE, &ADMIN_FRESH, &MEMBER_FRESH];

fn email(user: i64) -> String {
    format!("user-{user}@domains.itest")
}

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Composition {
    Off,
    Remote,
    RemoteUnavailable,
}

fn config(kratos: String, keto: String, composition: Composition) -> AppConfig {
    let mut config = AppConfig::default();
    config.telemetry.service_name = "sanvi-domains-routes".to_string();
    config.server.rate_limit.enabled = false;
    config.features.tenancy = true;
    config.features.identity = true;
    config.features.domains = composition != Composition::Off;
    config.tenancy.base_domain = "itest".to_string();
    config.ory.kratos_public_url = kratos.clone();
    config.ory.kratos_admin_url = kratos;
    config.ory.keto_read_url = keto.clone();
    config.ory.keto_write_url = keto;
    config
}

const KEY: &str = "domains-gateway-integration-key-0123456789";

async fn build(
    composition: Composition,
    pool: PgPool,
    upstream_counter: Arc<AtomicUsize>,
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
        config.domains_service.signing_secret = sanvi_telemetry::Redacted::new(KEY.into());
        config.domains_service.inbound_secret =
            sanvi_telemetry::Redacted::new("domains-gateway-delivery-key-9876543210".into());
        config.domains_service.base_url = if composition == Composition::RemoteUnavailable {
            "http://127.0.0.1:1".into()
        } else {
            let counter = upstream_counter.clone();
            spawn_stub(move |method, target, headers| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let token = headers
                        .get("x-domains-delegation")
                        .map(String::as_str)
                        .unwrap_or_default();
                    let purpose = if target.starts_with("/api/v1/platform") {
                        "platform"
                    } else {
                        "tenant"
                    };
                    let claims = Delegation::verify(
                        KEY,
                        token,
                        TENANT_AUDIENCE,
                        purpose,
                        &RequestBinding {
                            method: &method,
                            target: &target,
                            body: b"",
                            content_type: headers
                                .get("content-type")
                                .map(String::as_str)
                                .unwrap_or_default(),
                            if_match: headers
                                .get("if-match")
                                .map(String::as_str)
                                .unwrap_or_default(),
                        },
                    );
                    match claims {
                        Ok(claims) => {
                            let tenant_str = claims
                                .tenant
                                .map(|t| t.tenant_id.to_string())
                                .unwrap_or_default();
                            let user_str = claims
                                .subject
                                .map(|s| s.user_id.to_string())
                                .unwrap_or_default();
                            let body = json!({
                                "tenant": tenant_str,
                                "user": user_str,
                                "permission": claims.permission,
                                "target": target,
                                "items": [],
                                "cookie_forwarded": headers.contains_key("cookie"),
                                "authorization_forwarded": headers.contains_key("authorization"),
                            });
                            (200, "application/json", serde_json::to_vec(&body).unwrap())
                        }
                        Err(_) => (401, "application/json", b"{}".to_vec()),
                    }
                }
            })
            .await
        };
    }

    let metrics = MetricsRegistry::prometheus_only();
    let build_info = BuildInfo::new(
        "0.1.0-test".into(),
        "test".into(),
        "now".into(),
        "test".into(),
        "sanvi-domains-routes".into(),
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
        None, // domains (routes through remote gateway)
        None, // payments
        None, // advertising
        None, // accounting
        None, // redis
        None, // ssr_cache
        Arc::new(sanvi_resilience::DegradationRegistry::default()),
        None, // load_shed
    )
}

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
        .begin_unscoped("seed", "domains-routes")
        .await
        .unwrap();
    let tx = scope.tx_mut().as_inner();

    for (id, slug, status) in [
        (TENANT, "domains-a", "active"),
        (UNENTITLED_TENANT, "domains-b", "active"),
        (SUSPENDED_TENANT, "domains-c", "suspended"),
    ] {
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale, suspended_reason)
             VALUES ($1, $2, $2, $3, 'us', 'en', CASE WHEN $3 = 'suspended' THEN 'billing' ELSE NULL END)",
        )
        .bind(id)
        .bind(slug)
        .bind(status)
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
         VALUES ('domains.custom', 'custom domains', 'boolean', false),
                ('domains.purchase', 'domain purchase', 'boolean', false)
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
        (SUSPENDED_TENANT, OWNER, "tenant_owner"),
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

    // Grant entitlements for TENANT only
    let mut scope = manager.begin_for(TENANT).await.unwrap();
    sqlx::query(
        "INSERT INTO access.entitlements (tenant_id, feature_key, enabled, source)
         VALUES ($1, 'domains.custom', true, 'override'),
                ($1, 'domains.purchase', true, 'override')",
    )
    .bind(TENANT)
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();
}

async fn system_role(pool: &PgPool, key: &str) -> i64 {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager
        .begin_unscoped("seed", "domains-routes")
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
    fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
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
        let body = aw_test::read_body(response).await;
        Reply {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }
}

#[actix_web::test]
async fn remote_gateway_preserves_platform_security_and_delegates_only_verified_identity() {
    let db = sanvi_testsupport::test_db().await;
    seed(&db.app_pool()).await;
    let upstream_counter = Arc::new(AtomicUsize::new(0));
    let app =
        aw_test::init_service(build(Composition::Remote, db.app_pool(), upstream_counter).await)
            .await;

    // Authorized tenant owner gets 200 and verified claims forwarded
    let reply = Request::new("GET", DOMAINS).send(&app).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["tenant"], TENANT.to_string());
    assert_eq!(reply.json()["user"], OWNER.to_string());
    assert_eq!(reply.json()["permission"], "tenant.domains.read");
    assert_eq!(reply.json()["target"], DOMAINS);
    assert_eq!(reply.json()["cookie_forwarded"], false);
    assert_eq!(reply.json()["authorization_forwarded"], false);

    // Persona without permission (tenant_member) is denied 403 by the platform
    let reply = Request::new("GET", DOMAINS)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403);

    // Persona without permission cannot manage/claim
    let reply = Request::new("POST", DOMAINS)
        .persona(&MEMBER_FRESH)
        .json(json!({"hostname": "example.com"}))
        .send(&app)
        .await;
    assert_eq!(reply.status, 403);
}

#[actix_web::test]
async fn unentitled_tenant_rejected_at_gateway_and_service_never_called() {
    let db = sanvi_testsupport::test_db().await;
    seed(&db.app_pool()).await;
    let upstream_counter = Arc::new(AtomicUsize::new(0));
    let app = aw_test::init_service(
        build(Composition::Remote, db.app_pool(), upstream_counter.clone()).await,
    )
    .await;

    // 1. Claim custom domain requires connect entitlement ('domains.custom')
    let reply = Request::new("POST", DOMAINS)
        .host(UNENTITLED_HOST)
        .json(json!({"hostname": "custom.example.com"}))
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "claim domain response: {}", reply.body);
    assert_eq!(reply.json()["reason"], "domain_entitlement_required");

    // 2. Domain search requires purchase entitlement ('domains.purchase')
    let reply = Request::new("GET", format!("{SEARCH}?q=example.com"))
        .host(UNENTITLED_HOST)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "search response: {}", reply.body);
    assert_eq!(reply.json()["reason"], "domain_entitlement_required");

    // 3. Place domain order requires purchase entitlement
    let reply = Request::new("POST", ORDERS)
        .host(UNENTITLED_HOST)
        .json(json!({"domain": "example.com", "years": 1}))
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "place order response: {}", reply.body);
    assert_eq!(reply.json()["reason"], "domain_entitlement_required");

    // 4. List domain orders requires purchase entitlement
    let reply = Request::new("GET", ORDERS)
        .host(UNENTITLED_HOST)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "list orders response: {}", reply.body);
    assert_eq!(reply.json()["reason"], "domain_entitlement_required");

    // 5. Order auto-renew requires purchase entitlement
    let reply = Request::new("POST", format!("{ORDERS}/123/auto-renew"))
        .host(UNENTITLED_HOST)
        .json(json!({"auto_renew": true}))
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "auto renew response: {}", reply.body);
    assert_eq!(reply.json()["reason"], "domain_entitlement_required");

    // Proves the service was NEVER called for any of these unentitled requests!
    assert_eq!(
        upstream_counter.load(Ordering::SeqCst),
        0,
        "domains service was contacted for unentitled tenant!"
    );
}

#[actix_web::test]
async fn remote_outage_fails_closed_after_platform_checks() {
    let db = sanvi_testsupport::test_db().await;
    seed(&db.app_pool()).await;
    let upstream_counter = Arc::new(AtomicUsize::new(0));
    let app = aw_test::init_service(
        build(
            Composition::RemoteUnavailable,
            db.app_pool(),
            upstream_counter,
        )
        .await,
    )
    .await;

    // Authorized user against unreachable service gets 503
    let reply = Request::new("GET", DOMAINS).send(&app).await;
    assert_eq!(reply.status, 503);

    // Unauthorized user gets 403 BEFORE any attempt to contact service
    let reply = Request::new("GET", DOMAINS)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403);
}

#[actix_web::test]
async fn tenant_state_internal_endpoint() {
    const SECRET: &str = "domains-tenant-state-secret-0123456789";
    let db = sanvi_testsupport::test_db().await;
    seed(&db.app_pool()).await;
    let ids = sanvi_testsupport::test_ids();
    let tx = TxManager::new(db.app_pool(), ids.clone());
    let mut config = AppConfig::default();
    config.tenancy.base_domain = "itest".to_string();
    let tenancy = build_tenancy_state(db.app_pool(), ids.clone(), None, &config.tenancy, None);

    let state = sanvi_api::domains::DomainsIngestState {
        secret: sanvi_telemetry::Redacted::new(SECRET.into()),
        transactions: tx,
        writer: Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(ids)),
        resolve_tenant: tenancy.resolve,
    };
    let app = aw_test::init_service(
        actix_web::App::new()
            .app_data(actix_web::web::Data::new(state))
            .route(
                "/internal/domains/tenant-state",
                actix_web::web::post().to(sanvi_api::domains::tenant_state),
            ),
    )
    .await;

    // Active tenant returns active: true, status: "active"
    let body = serde_json::to_vec(&json!({
        "tenant_id": TENANT.to_string()
    }))
    .unwrap();
    let claims = Delegation::new(
        AUDIT_AUDIENCE,
        "tenant-state",
        &RequestBinding::json("POST", "/internal/domains/tenant-state", &body),
    );
    let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/domains/tenant-state")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, token))
        .set_payload(body)
        .to_request();
    let response = aw_test::call_service(&app, request).await;
    assert_eq!(response.status().as_u16(), 200);
    let resp_bytes = aw_test::read_body(response).await;
    let resp_json: Value = serde_json::from_slice(&resp_bytes).unwrap();
    assert_eq!(resp_json["active"], true);
    assert_eq!(resp_json["status"], "active");

    // Suspended tenant returns active: false, status: "suspended"
    let body = serde_json::to_vec(&json!({
        "tenant_id": SUSPENDED_TENANT.to_string()
    }))
    .unwrap();
    let claims = Delegation::new(
        AUDIT_AUDIENCE,
        "tenant-state",
        &RequestBinding::json("POST", "/internal/domains/tenant-state", &body),
    );
    let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/domains/tenant-state")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, token))
        .set_payload(body)
        .to_request();
    let response = aw_test::call_service(&app, request).await;
    assert_eq!(response.status().as_u16(), 200);
    let resp_bytes = aw_test::read_body(response).await;
    let resp_json: Value = serde_json::from_slice(&resp_bytes).unwrap();
    assert_eq!(resp_json["active"], false);
    assert_eq!(resp_json["status"], "suspended");

    // Unknown tenant returns active: false, status: null
    let body = serde_json::to_vec(&json!({
        "tenant_id": "999999999"
    }))
    .unwrap();
    let claims = Delegation::new(
        AUDIT_AUDIENCE,
        "tenant-state",
        &RequestBinding::json("POST", "/internal/domains/tenant-state", &body),
    );
    let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/domains/tenant-state")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, token))
        .set_payload(body)
        .to_request();
    let response = aw_test::call_service(&app, request).await;
    assert_eq!(response.status().as_u16(), 200);
    let resp_bytes = aw_test::read_body(response).await;
    let resp_json: Value = serde_json::from_slice(&resp_bytes).unwrap();
    assert_eq!(resp_json["active"], false);
    assert_eq!(resp_json["status"], Value::Null);

    // Invalid delegation token returns 401
    let request = aw_test::TestRequest::post()
        .uri("/internal/domains/tenant-state")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, "invalid-token"))
        .set_payload(b"{}".to_vec())
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        401
    );
}

#[actix_web::test]
async fn remote_audit_and_event_retries_commit_one_platform_effect() {
    const SECRET: &str = "domains-ingestion-secret-0123456789";
    let db = sanvi_testsupport::test_db().await;
    let ids = sanvi_testsupport::test_ids();
    let tx = TxManager::new(db.app_pool(), ids.clone());
    let mut config = AppConfig::default();
    config.tenancy.base_domain = "itest".to_string();
    let tenancy = build_tenancy_state(db.app_pool(), ids.clone(), None, &config.tenancy, None);

    let state = sanvi_api::domains::DomainsIngestState {
        secret: sanvi_telemetry::Redacted::new(SECRET.into()),
        transactions: tx,
        writer: Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(ids)),
        resolve_tenant: tenancy.resolve,
    };
    let app = aw_test::init_service(
        actix_web::App::new()
            .app_data(actix_web::web::Data::new(state))
            .route(
                "/internal/domains/audit",
                actix_web::web::post().to(sanvi_api::domains::ingest_domains_audit),
            )
            .route(
                "/internal/domains/events",
                actix_web::web::post().to(sanvi_api::domains::ingest_domains_event),
            ),
    )
    .await;

    let entry = sanvi_audit::AuditEntryData {
        occurred_at: Utc::now(),
        actor_type: sanvi_audit::ActorType::User,
        actor_id: Some(sanvi_kernel::UserId::try_from(OWNER).unwrap()),
        on_behalf_of: None,
        tenant_id: Some(sanvi_kernel::TenantId::try_from(TENANT).unwrap()),
        action: "domains.test".into(),
        resource_type: Some("custom_domain".into()),
        resource_id: Some("123".into()),
        before: None,
        after: Some(json!({"hostname": "example.com"})),
        request_id: Some("domains-remote-request".into()),
        ip: None,
        user_agent: None,
    };

    for (path, purpose, body) in [
        (
            "/internal/domains/audit",
            "audit",
            serde_json::to_vec(&AuditCommand {
                id: 919_200_001,
                entry: entry.clone(),
            })
            .unwrap(),
        ),
        (
            "/internal/domains/events",
            "event",
            serde_json::to_vec(&sanvi_messaging::OutboxRecord {
                id: sanvi_kernel::EventId::try_from(919_000_002).unwrap(),
                aggregate: "domains.domain".into(),
                aggregate_id: sanvi_kernel::SnowflakeId::try_from(123).unwrap(),
                event_type: "domains.verified".into(),
                payload: json!({"domain_id": "123"}),
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

        // First delivery + idempotent retry
        for _ in 0..2 {
            let request = aw_test::TestRequest::post()
                .uri(path)
                .insert_header(("content-type", "application/json"))
                .insert_header((DELEGATION_HEADER, token.clone()))
                .set_payload(body.clone())
                .to_request();
            let response = aw_test::call_service(&app, request).await;
            let status = response.status().as_u16();
            let resp_body = aw_test::read_body(response).await;
            assert_eq!(
                status,
                204,
                "{path}: {}",
                String::from_utf8_lossy(&resp_body)
            );
        }

        // Invalid token denied
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

    // New audit command with new ID appends
    let send_audit = |command: AuditCommand| {
        let body = serde_json::to_vec(&command).unwrap();
        let claims = Delegation::new(
            AUDIT_AUDIENCE,
            "audit",
            &RequestBinding::json("POST", "/internal/domains/audit", &body),
        );
        aw_test::TestRequest::post()
            .uri("/internal/domains/audit")
            .insert_header(("content-type", "application/json"))
            .insert_header((
                DELEGATION_HEADER,
                sanvi_crypto::sign(SECRET.as_bytes(), &claims),
            ))
            .set_payload(body)
            .to_request()
    };
    let twin = send_audit(AuditCommand {
        id: 919_200_002,
        entry: entry.clone(),
    });
    assert_eq!(
        aw_test::call_service(&app, twin).await.status().as_u16(),
        204
    );

    // Reused audit command ID with different content is refused 409
    let mut altered = entry;
    altered.request_id = Some("altered-request".into());
    let reused = send_audit(AuditCommand {
        id: 919_200_001,
        entry: altered,
    });
    assert_eq!(
        aw_test::call_service(&app, reused).await.status().as_u16(),
        409
    );

    let admin = db.admin_pool().await;
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit.entries WHERE action = 'domains.test'")
            .fetch_one(&admin)
            .await
            .unwrap();
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform.outbox WHERE id = 919000002")
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(audits, 2);
    assert_eq!(events, 1);
}

#[actix_web::test]
async fn remote_storefront_tenant_lookup_behavior() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let counter = call_count.clone();

    let stub_url = spawn_stub(move |_method, target, headers| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let token = headers
                .get("x-domains-delegation")
                .map(String::as_str)
                .unwrap_or_default();
            if Delegation::verify(
                KEY,
                token,
                TENANT_AUDIENCE,
                "platform",
                &RequestBinding::json("GET", &target, b""),
            )
            .is_err()
            {
                return (401, "application/json", b"{}".to_vec());
            }
            if target.contains("host=shop.example.com") {
                (
                    200,
                    "application/json",
                    serde_json::to_vec(&json!({"tenant_id": TENANT.to_string()})).unwrap(),
                )
            } else if target.contains("host=missing.example.com") {
                (404, "application/json", b"{}".to_vec())
            } else {
                (503, "application/json", b"{}".to_vec())
            }
        }
    })
    .await;

    let service_config = ServiceConfig {
        base_url: stub_url,
        signing_secret: sanvi_telemetry::Redacted::new(KEY.into()),
        inbound_secret: sanvi_telemetry::Redacted::new(
            "inbound-key-012345678901234567890123".into(),
        ),
    };
    let remote_client =
        sanvi_domains::adapters::remote::RemoteClient::new(&service_config).unwrap();
    let cache = Arc::new(LiveDomainCache::new(None, 300));
    let source = Arc::new(sanvi_domains::adapters::RemoteHostnameSource::new(
        remote_client,
    ));
    let lookup_service = LiveDomainLookupService::new(cache, source);

    // 1. Successful lookup returns tenant ID
    let resolved = lookup_service.resolve("shop.example.com").await;
    assert_eq!(
        resolved,
        Some(sanvi_kernel::TenantId::try_from(TENANT).unwrap())
    );
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    // 2. Second lookup hits cache, no new remote call
    let cached = lookup_service.resolve("shop.example.com").await;
    assert_eq!(
        cached,
        Some(sanvi_kernel::TenantId::try_from(TENANT).unwrap())
    );
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    // 3. 404 (unknown host) returns None and caches negative hit
    let missing = lookup_service.resolve("missing.example.com").await;
    assert_eq!(missing, None);
    assert_eq!(call_count.load(Ordering::SeqCst), 2);

    let missing_cached = lookup_service.resolve("missing.example.com").await;
    assert_eq!(missing_cached, None);
    assert_eq!(call_count.load(Ordering::SeqCst), 2);

    // 4. Remote outage (503) fails closed (None) without negative caching
    let err = lookup_service.resolve("error.example.com").await;
    assert_eq!(err, None);
    assert_eq!(call_count.load(Ordering::SeqCst), 3);

    // Since outage was not cached, next call tries remote again
    let err_retry = lookup_service.resolve("error.example.com").await;
    assert_eq!(err_retry, None);
    assert_eq!(call_count.load(Ordering::SeqCst), 4);
}
