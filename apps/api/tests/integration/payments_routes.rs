//! Tenant payments routes against the assembled app and a real Postgres.
//! Mirrors the precedent in domains_routes.rs.

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
use sanvi_payments::adapters::{
    RemotePaymentsPrivacyProvider,
    remote::{
        AUDIT_AUDIENCE, AuditCommand, DELEGATION_HEADER, Delegation, RequestBinding, ServiceConfig,
        TENANT_AUDIENCE,
    },
};
use sanvi_persistence::{PgPool, TxManager};
use sanvi_privacy::domain::{
    DataBundle, DataClass, DataEntry, EraseMode, EraseReport, PersonalDataProvider, SubjectRef,
};
use sanvi_system::domain::BuildInfo;
use sanvi_telemetry::MetricsRegistry;

use super::support::{StubReply, spawn_stub};

// The 910_243_xxx range is owned by this suite.
const TENANT: i64 = 910_243_001;
const UNENTITLED_TENANT: i64 = 910_243_002;
const SUSPENDED_TENANT: i64 = 910_243_003;
const OWNER: i64 = 910_243_010;
const ADMIN: i64 = 910_243_011;
const MEMBER: i64 = 910_243_012;

const HOST: &str = "payments-a.itest";
const UNENTITLED_HOST: &str = "payments-b.itest";
const SESSION_COOKIE: &str = "ory_kratos_session";

const PAYMENTS: &str = "/api/v1/tenant/payments";
const PROVIDERS: &str = "/api/v1/tenant/payments/providers";
const TAX_SETTINGS: &str = "/api/v1/tenant/payments/tax-settings";
const REFUND: &str = "/api/v1/tenant/payments/910243100/refund";
const CHECKOUT: &str = "/api/v1/tenant/checkout";
const CHECKOUT_CONFIG: &str = "/api/v1/tenant/checkout/config";

const SIGNING_KEY: &str = "payments-gateway-signing-secret-0123456789";
const INBOUND_KEY: &str = "payments-gateway-inbound-secret-9876543210";

struct Persona {
    token: &'static str,
    user: i64,
    kratos_id: &'static str,
    fresh: bool,
}

const OWNER_FRESH: Persona = Persona {
    token: "payments-owner-fresh",
    user: OWNER,
    kratos_id: "91024300-0000-7000-8000-00000000a010",
    fresh: true,
};
const OWNER_STALE: Persona = Persona {
    token: "payments-owner-stale",
    user: OWNER,
    kratos_id: "91024300-0000-7000-8000-00000000a010",
    fresh: false,
};
const ADMIN_FRESH: Persona = Persona {
    token: "payments-admin-fresh",
    user: ADMIN,
    kratos_id: "91024300-0000-7000-8000-00000000a011",
    fresh: true,
};
const MEMBER_FRESH: Persona = Persona {
    token: "payments-member-fresh",
    user: MEMBER,
    kratos_id: "91024300-0000-7000-8000-00000000a012",
    fresh: true,
};
const PERSONAS: [&Persona; 4] = [&OWNER_FRESH, &OWNER_STALE, &ADMIN_FRESH, &MEMBER_FRESH];

fn email(user: i64) -> String {
    format!("user-{user}@payments.itest")
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
    config.telemetry.service_name = "sanvi-payments-routes".to_string();
    config.server.rate_limit.enabled = false;
    config.features.tenancy = true;
    config.features.identity = true;
    config.features.payments = composition != Composition::Off;
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
        config.payments_service.signing_secret = sanvi_telemetry::Redacted::new(SIGNING_KEY.into());
        config.payments_service.inbound_secret = sanvi_telemetry::Redacted::new(INBOUND_KEY.into());
        config.payments_service.base_url = if composition == Composition::RemoteUnavailable {
            "http://127.0.0.1:1".into()
        } else {
            let counter = upstream_counter.clone();
            spawn_stub(move |method, target, headers| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let token = headers
                        .get(DELEGATION_HEADER)
                        .map(String::as_str)
                        .unwrap_or_default();
                    let binding = RequestBinding {
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
                    };
                    let claims =
                        Delegation::verify(SIGNING_KEY, token, TENANT_AUDIENCE, "tenant", &binding);
                    match claims {
                        Ok(claims) => {
                            let tenant_id = claims
                                .tenant
                                .map(|t| t.tenant_id.as_i64())
                                .unwrap_or_default();
                            let user_id = claims
                                .subject
                                .as_ref()
                                .map(|s| s.user_id.as_i64())
                                .unwrap_or_default();
                            let role_ids_empty = claims
                                .subject
                                .as_ref()
                                .map(|s| s.role_ids.is_empty())
                                .unwrap_or(false);
                            let status = if target == CHECKOUT && method == "POST" {
                                201
                            } else {
                                200
                            };
                            let body = json!({
                                "tenant_id": tenant_id,
                                "user_id": user_id,
                                "role_ids_empty": role_ids_empty,
                                "permission": claims.permission,
                                "can_read": claims.can_read,
                                "target": target,
                                "method": method,
                                "cookie_forwarded": headers.contains_key("cookie"),
                                "authorization_forwarded": headers.contains_key("authorization"),
                                "idempotency_key": claims.idempotency_key,
                                "idempotency_header": headers.get("idempotency-key"),
                                "client_ip": claims.client_ip.map(|ip| ip.to_string()),
                                "echo_marker": "payments-standin-relay-ok",
                            });
                            (
                                status,
                                "application/json",
                                serde_json::to_vec(&body).unwrap(),
                            )
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
        "sanvi-payments-routes".into(),
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
        .begin_unscoped("seed", "payments-routes")
        .await
        .unwrap();
    let tx = scope.tx_mut().as_inner();

    for (id, slug, status) in [
        (TENANT, "payments-a", "active"),
        (UNENTITLED_TENANT, "payments-b", "active"),
        (SUSPENDED_TENANT, "payments-c", "suspended"),
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
         VALUES ('payments.stripe_connect', 'Stripe Connect', 'boolean', true)
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

    // Revoke payments.stripe_connect for UNENTITLED_TENANT with an override
    let mut scope = manager.begin_for(UNENTITLED_TENANT).await.unwrap();
    sqlx::query(
        "INSERT INTO access.entitlements (tenant_id, feature_key, enabled, source)
         VALUES ($1, 'payments.stripe_connect', false, 'override')",
    )
    .bind(UNENTITLED_TENANT)
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();
}

async fn system_role(pool: &PgPool, key: &str) -> i64 {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager
        .begin_unscoped("seed", "payments-routes")
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
    unauthenticated: bool,
    idempotency_key: Option<&'a str>,
    peer: Option<&'a str>,
}

impl<'a> Request<'a> {
    fn new(method: &'a str, uri: impl Into<String>) -> Self {
        Self {
            method,
            uri: uri.into(),
            persona: &OWNER_FRESH,
            host: HOST,
            unauthenticated: false,
            idempotency_key: None,
            peer: None,
        }
    }
    fn idempotency_key(mut self, key: &'a str) -> Self {
        self.idempotency_key = Some(key);
        self
    }
    fn peer(mut self, peer: &'a str) -> Self {
        self.peer = Some(peer);
        self
    }
    fn persona(mut self, persona: &'a Persona) -> Self {
        self.persona = persona;
        self
    }
    fn host(mut self, host: &'a str) -> Self {
        self.host = host;
        self
    }
    fn unauthenticated(mut self) -> Self {
        self.unauthenticated = true;
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
            .insert_header((actix_web::http::header::HOST, self.host));
        if let Some(key) = self.idempotency_key {
            builder = builder.insert_header(("idempotency-key", key));
        }
        if let Some(peer) = self.peer {
            builder = builder.peer_addr(peer.parse().unwrap());
        }
        if !self.unauthenticated {
            builder = builder.cookie(actix_web::cookie::Cookie::new(
                SESSION_COOKIE,
                self.persona.token,
            ));
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn remote_gateway_delegates_verified_identity_with_route_permission() {
    let db = sanvi_testsupport::test_db().await;
    seed(&db.app_pool()).await;
    let upstream_counter = Arc::new(AtomicUsize::new(0));
    let app = aw_test::init_service(
        build(Composition::Remote, db.app_pool(), upstream_counter.clone()).await,
    )
    .await;

    // 1. GET /api/v1/tenant/payments (payments.read)
    let reply = Request::new("GET", PAYMENTS).send(&app).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["echo_marker"], "payments-standin-relay-ok");
    assert_eq!(reply.json()["tenant_id"], TENANT);
    assert_eq!(reply.json()["user_id"], OWNER);
    assert_eq!(reply.json()["role_ids_empty"], true);
    assert_eq!(reply.json()["permission"], "payments.read");
    assert_eq!(reply.json()["can_read"], true);
    assert_eq!(reply.json()["target"], PAYMENTS);
    assert_eq!(reply.json()["cookie_forwarded"], false);
    assert_eq!(reply.json()["authorization_forwarded"], false);

    // 2. PUT /api/v1/tenant/payments/tax-settings (payments.manage)
    let reply = Request::new("PUT", TAX_SETTINGS).send(&app).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["echo_marker"], "payments-standin-relay-ok");
    assert_eq!(reply.json()["tenant_id"], TENANT);
    assert_eq!(reply.json()["user_id"], OWNER);
    assert_eq!(reply.json()["role_ids_empty"], true);
    assert_eq!(reply.json()["permission"], "payments.manage");
    assert_eq!(reply.json()["can_read"], true);
    assert_eq!(reply.json()["target"], TAX_SETTINGS);

    // 3. POST /api/v1/tenant/payments/{id}/refund (payments.refund)
    // The caller's Idempotency-Key and address travel as signed claims, and
    // the key is forwarded as the header the service's handlers read.
    let key = "0192b1c4-6a7e-7c3d-9f00-0000000000aa";
    let reply = Request::new("POST", REFUND)
        .idempotency_key(key)
        .peer("198.51.100.23:51000")
        .send(&app)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["idempotency_key"], key);
    assert_eq!(reply.json()["idempotency_header"], key);
    assert_eq!(reply.json()["client_ip"], "198.51.100.23");
    assert_eq!(reply.json()["echo_marker"], "payments-standin-relay-ok");
    assert_eq!(reply.json()["tenant_id"], TENANT);
    assert_eq!(reply.json()["user_id"], OWNER);
    assert_eq!(reply.json()["role_ids_empty"], true);
    assert_eq!(reply.json()["permission"], "payments.refund");
    assert_eq!(reply.json()["can_read"], true);
    assert_eq!(reply.json()["target"], REFUND);

    // 4. POST /api/v1/tenant/checkout (payments.checkout) — stand-in returns 201
    let reply = Request::new("POST", CHECKOUT).send(&app).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.json()["echo_marker"], "payments-standin-relay-ok");
    assert_eq!(reply.json()["tenant_id"], TENANT);
    assert_eq!(reply.json()["user_id"], OWNER);
    assert_eq!(reply.json()["role_ids_empty"], true);
    assert_eq!(reply.json()["permission"], "payments.checkout");
    assert_eq!(reply.json()["can_read"], true);
    assert_eq!(reply.json()["target"], CHECKOUT);

    // 5. GET /api/v1/tenant/checkout/config (payments.checkout)
    let reply = Request::new("GET", CHECKOUT_CONFIG).send(&app).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["echo_marker"], "payments-standin-relay-ok");
    assert_eq!(reply.json()["tenant_id"], TENANT);
    assert_eq!(reply.json()["user_id"], OWNER);
    assert_eq!(reply.json()["role_ids_empty"], true);
    assert_eq!(reply.json()["permission"], "payments.checkout");
    assert_eq!(reply.json()["can_read"], true);
    assert_eq!(reply.json()["target"], CHECKOUT_CONFIG);

    assert_eq!(upstream_counter.load(Ordering::SeqCst), 5);
}

#[actix_web::test]
async fn member_without_permission_is_rejected_at_gateway_and_service_never_called() {
    let db = sanvi_testsupport::test_db().await;
    seed(&db.app_pool()).await;
    let upstream_counter = Arc::new(AtomicUsize::new(0));
    let app = aw_test::init_service(
        build(Composition::Remote, db.app_pool(), upstream_counter.clone()).await,
    )
    .await;

    // Member lacks payments.read
    let reply = Request::new("GET", PAYMENTS)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);

    // Member lacks payments.manage
    let reply = Request::new("PUT", TAX_SETTINGS)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);

    // Member lacks payments.refund
    let reply = Request::new("POST", REFUND)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);

    // Member lacks payments.checkout
    let reply = Request::new("POST", CHECKOUT)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);

    // Member lacks payments.checkout
    let reply = Request::new("GET", CHECKOUT_CONFIG)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);

    // Admin lacks payments.refund (refunds are owner-only by default)
    let reply = Request::new("POST", REFUND)
        .persona(&ADMIN_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);

    // Upstream service was never contacted for any of these
    assert_eq!(
        upstream_counter.load(Ordering::SeqCst),
        0,
        "Payments service was contacted for unauthorized request!"
    );
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

    // Entitlement-gated routes rejected with 403 for unentitled tenant
    for (method, uri) in [
        ("GET", PAYMENTS),
        ("PUT", TAX_SETTINGS),
        ("POST", REFUND),
        ("POST", CHECKOUT),
        ("GET", CHECKOUT_CONFIG),
    ] {
        let reply = Request::new(method, uri)
            .host(UNENTITLED_HOST)
            .send(&app)
            .await;
        assert_eq!(
            reply.status, 403,
            "expected 403 for {method} {uri}, got: {}",
            reply.body
        );
        assert!(
            reply.body.contains("payments.stripe_connect"),
            "expected detail to name payments.stripe_connect, got: {}",
            reply.body
        );
    }

    // Upstream service was never contacted for any of these unentitled calls
    assert_eq!(
        upstream_counter.load(Ordering::SeqCst),
        0,
        "Payments service was contacted for unentitled tenant!"
    );

    // Catalog route GET /payments/providers is NOT entitlement-gated; reaches service
    let reply = Request::new("GET", PROVIDERS)
        .host(UNENTITLED_HOST)
        .send(&app)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["echo_marker"], "payments-standin-relay-ok");
    assert_eq!(reply.json()["tenant_id"], UNENTITLED_TENANT);
    assert_eq!(reply.json()["permission"], "payments.read");
    assert_eq!(upstream_counter.load(Ordering::SeqCst), 1);
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

    // 1. Unauthenticated request gets 401 first, before touching remote
    let reply = Request::new("GET", PAYMENTS)
        .unauthenticated()
        .send(&app)
        .await;
    assert_eq!(reply.status, 401, "{}", reply.body);

    // 2. Member without permission gets 403 first, before touching remote
    let reply = Request::new("GET", PAYMENTS)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);

    // 3. Authorized tenant owner against unreachable service fails closed with 503
    let reply = Request::new("GET", PAYMENTS).send(&app).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
}

#[actix_web::test]
async fn remote_audit_and_event_retries_commit_one_platform_effect() {
    let db = sanvi_testsupport::test_db().await;
    let ids = sanvi_testsupport::test_ids();
    let tx = TxManager::new(db.app_pool(), ids.clone());

    let state = sanvi_api::payments::PaymentsIngestState {
        secret: sanvi_telemetry::Redacted::new(INBOUND_KEY.into()),
        transactions: tx,
        writer: Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(ids)),
    };
    let app = aw_test::init_service(
        actix_web::App::new()
            .app_data(actix_web::web::Data::new(state))
            .configure(sanvi_api::payments::internal_routes),
    )
    .await;

    let audit_cmd_id = 919_243_001_i64;
    let event_id = 919_243_002_i64;

    let entry = sanvi_audit::AuditEntryData {
        occurred_at: Utc::now(),
        actor_type: sanvi_audit::ActorType::User,
        actor_id: Some(sanvi_kernel::UserId::try_from(OWNER).unwrap()),
        on_behalf_of: None,
        tenant_id: Some(sanvi_kernel::TenantId::try_from(TENANT).unwrap()),
        action: "payments.refund.created".into(),
        resource_type: Some("payment_refund".into()),
        resource_id: Some("123".into()),
        before: None,
        after: Some(json!({"amount": 1000})),
        request_id: Some("payments-remote-audit-req".into()),
        ip: None,
        user_agent: None,
    };

    let audit_body = serde_json::to_vec(&AuditCommand {
        id: audit_cmd_id,
        entry: entry.clone(),
    })
    .unwrap();

    let event_record = sanvi_messaging::OutboxRecord {
        id: sanvi_kernel::EventId::try_from(event_id).unwrap(),
        aggregate: "payments.payment".into(),
        aggregate_id: sanvi_kernel::SnowflakeId::try_from(123).unwrap(),
        event_type: "payments.captured".into(),
        payload: json!({"payment_id": 123}),
        metadata: json!({}),
        occurred_at: Utc::now(),
        published_at: None,
        attempts: 0,
    };
    let event_body = serde_json::to_vec(&event_record).unwrap();

    // 1. Audit delivery + idempotent retry commit one effect
    let audit_claims = Delegation::new(
        AUDIT_AUDIENCE,
        "audit",
        &RequestBinding::json("POST", "/internal/payments/audit", &audit_body),
    );
    let audit_token = sanvi_crypto::sign(INBOUND_KEY.as_bytes(), &audit_claims);

    for _ in 0..2 {
        let request = aw_test::TestRequest::post()
            .uri("/internal/payments/audit")
            .insert_header(("content-type", "application/json"))
            .insert_header((DELEGATION_HEADER, audit_token.clone()))
            .set_payload(audit_body.clone())
            .to_request();
        let response = aw_test::call_service(&app, request).await;
        assert_eq!(response.status().as_u16(), 204);
    }

    // Same audit id with different content returns 409
    let mut altered_entry = entry.clone();
    altered_entry.request_id = Some("altered-request".into());
    let altered_audit_body = serde_json::to_vec(&AuditCommand {
        id: audit_cmd_id,
        entry: altered_entry,
    })
    .unwrap();
    let altered_claims = Delegation::new(
        AUDIT_AUDIENCE,
        "audit",
        &RequestBinding::json("POST", "/internal/payments/audit", &altered_audit_body),
    );
    let altered_token = sanvi_crypto::sign(INBOUND_KEY.as_bytes(), &altered_claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/payments/audit")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, altered_token))
        .set_payload(altered_audit_body)
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        409
    );

    // Wrong purpose on audit returns 401
    let wrong_purpose_claims = Delegation::new(
        AUDIT_AUDIENCE,
        "event",
        &RequestBinding::json("POST", "/internal/payments/audit", &audit_body),
    );
    let wrong_purpose_token = sanvi_crypto::sign(INBOUND_KEY.as_bytes(), &wrong_purpose_claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/payments/audit")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, wrong_purpose_token))
        .set_payload(audit_body.clone())
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        401
    );

    // Wrong secret on audit returns 401
    let wrong_secret = "wrong-secret-012345678901234567890123";
    let wrong_secret_token = sanvi_crypto::sign(wrong_secret.as_bytes(), &audit_claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/payments/audit")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, wrong_secret_token))
        .set_payload(audit_body.clone())
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        401
    );

    // Action not starting with "payments." returns 403
    let mut invalid_action_entry = entry.clone();
    invalid_action_entry.action = "domains.verified".into();
    let invalid_action_body = serde_json::to_vec(&AuditCommand {
        id: 919_243_003,
        entry: invalid_action_entry,
    })
    .unwrap();
    let invalid_action_claims = Delegation::new(
        AUDIT_AUDIENCE,
        "audit",
        &RequestBinding::json("POST", "/internal/payments/audit", &invalid_action_body),
    );
    let invalid_action_token = sanvi_crypto::sign(INBOUND_KEY.as_bytes(), &invalid_action_claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/payments/audit")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, invalid_action_token))
        .set_payload(invalid_action_body)
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        403
    );

    // 2. Event delivery + idempotent retry commit one effect
    let event_claims = Delegation::new(
        AUDIT_AUDIENCE,
        "event",
        &RequestBinding::json("POST", "/internal/payments/events", &event_body),
    );
    let event_token = sanvi_crypto::sign(INBOUND_KEY.as_bytes(), &event_claims);

    for _ in 0..2 {
        let request = aw_test::TestRequest::post()
            .uri("/internal/payments/events")
            .insert_header(("content-type", "application/json"))
            .insert_header((DELEGATION_HEADER, event_token.clone()))
            .set_payload(event_body.clone())
            .to_request();
        let response = aw_test::call_service(&app, request).await;
        assert_eq!(response.status().as_u16(), 204);
    }

    // Conflicting event reuse with different content returns 409
    let mut altered_event = event_record.clone();
    altered_event.payload = json!({"payment_id": 999});
    let altered_event_body = serde_json::to_vec(&altered_event).unwrap();
    let altered_event_claims = Delegation::new(
        AUDIT_AUDIENCE,
        "event",
        &RequestBinding::json("POST", "/internal/payments/events", &altered_event_body),
    );
    let altered_event_token = sanvi_crypto::sign(INBOUND_KEY.as_bytes(), &altered_event_claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/payments/events")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, altered_event_token))
        .set_payload(altered_event_body)
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        409
    );

    // Wrong purpose on events returns 401
    let wrong_purpose_event_claims = Delegation::new(
        AUDIT_AUDIENCE,
        "audit",
        &RequestBinding::json("POST", "/internal/payments/events", &event_body),
    );
    let wrong_purpose_event_token =
        sanvi_crypto::sign(INBOUND_KEY.as_bytes(), &wrong_purpose_event_claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/payments/events")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, wrong_purpose_event_token))
        .set_payload(event_body.clone())
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        401
    );

    // Wrong secret on events returns 401
    let wrong_secret_event_token = sanvi_crypto::sign(wrong_secret.as_bytes(), &event_claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/payments/events")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, wrong_secret_event_token))
        .set_payload(event_body.clone())
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        401
    );

    // Event aggregate not starting with "payments." returns 403
    let mut invalid_aggregate_event = event_record.clone();
    invalid_aggregate_event.id = sanvi_kernel::EventId::try_from(919_243_004).unwrap();
    invalid_aggregate_event.aggregate = "domains.domain".into();
    let invalid_aggregate_body = serde_json::to_vec(&invalid_aggregate_event).unwrap();
    let invalid_aggregate_claims = Delegation::new(
        AUDIT_AUDIENCE,
        "event",
        &RequestBinding::json("POST", "/internal/payments/events", &invalid_aggregate_body),
    );
    let invalid_aggregate_token =
        sanvi_crypto::sign(INBOUND_KEY.as_bytes(), &invalid_aggregate_claims);
    let request = aw_test::TestRequest::post()
        .uri("/internal/payments/events")
        .insert_header(("content-type", "application/json"))
        .insert_header((DELEGATION_HEADER, invalid_aggregate_token))
        .set_payload(invalid_aggregate_body)
        .to_request();
    assert_eq!(
        aw_test::call_service(&app, request).await.status().as_u16(),
        403
    );

    // Verify DB states: exactly 1 audit receipt, 1 audit entry, 1 event receipt, 1 outbox record
    let admin = db.admin_pool().await;
    let audit_receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit.payments_receipts WHERE command_id = $1")
            .bind(audit_cmd_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(audit_receipts, 1);

    let audit_entries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit.entries WHERE action = 'payments.refund.created'",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(audit_entries, 1);

    let event_receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.payments_event_receipts WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(event_receipts, 1);

    let outbox_entries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform.outbox WHERE id = $1")
            .bind(event_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(outbox_entries, 1);
}

#[actix_web::test]
async fn remote_privacy_provider_calls_service_and_fails_closed() {
    let subject = SubjectRef::contact("user@privacy.itest");

    // 1. Provider against stand-in verifying purpose "privacy" and returning DataBundle / EraseReport
    let stub_url = spawn_stub(move |method, target, headers| async move {
        let token = headers
            .get(DELEGATION_HEADER)
            .map(String::as_str)
            .unwrap_or_default();
        let expected_subject = SubjectRef::contact("user@privacy.itest");
        let body = match target.as_str() {
            "/internal/privacy/collect" => serde_json::to_vec(&expected_subject).unwrap(),
            "/internal/privacy/erase" => {
                #[derive(serde::Serialize)]
                struct EraseReq<'a> {
                    subject: &'a SubjectRef,
                    mode: EraseMode,
                }
                serde_json::to_vec(&EraseReq {
                    subject: &expected_subject,
                    mode: EraseMode::Full,
                })
                .unwrap()
            }
            _ => vec![],
        };
        let claims = Delegation::verify(
            SIGNING_KEY,
            token,
            TENANT_AUDIENCE,
            "privacy",
            &RequestBinding::json(&method, &target, &body),
        );
        if claims.is_err() {
            return (401, "application/json", b"{}".to_vec());
        }

        match target.as_str() {
            "/internal/privacy/collect" => {
                let bundle = DataBundle {
                    context: "payments".into(),
                    entries: vec![DataEntry {
                        data_class: DataClass::Billing,
                        category: "Financial".into(),
                        label: "test_entry".into(),
                        value: json!({"checkout_id": 123}),
                    }],
                };
                (
                    200,
                    "application/json",
                    serde_json::to_vec(&bundle).unwrap(),
                )
            }
            "/internal/privacy/erase" => {
                let report = EraseReport {
                    context: "payments".into(),
                    erased: 1,
                    anonymised: 0,
                    retained: vec![],
                };
                (
                    200,
                    "application/json",
                    serde_json::to_vec(&report).unwrap(),
                )
            }
            _ => (404, "application/json", b"{}".to_vec()),
        }
    })
    .await;

    let service_config = ServiceConfig {
        base_url: stub_url,
        signing_secret: sanvi_telemetry::Redacted::new(SIGNING_KEY.into()),
        inbound_secret: sanvi_telemetry::Redacted::new(INBOUND_KEY.into()),
    };
    let provider = RemotePaymentsPrivacyProvider::new(&service_config).unwrap();

    let bundle = provider.collect(&subject).await.unwrap();
    assert_eq!(bundle.context, "payments");
    assert_eq!(bundle.entries.len(), 1);
    assert_eq!(bundle.entries[0].label, "test_entry");

    let report = provider.erase(&subject, EraseMode::Full).await.unwrap();
    assert_eq!(report.context, "payments");
    assert_eq!(report.erased, 1);

    // 2. Provider against closed port returns an error (never an empty success)
    let closed_config = ServiceConfig {
        base_url: "http://127.0.0.1:1".into(),
        signing_secret: sanvi_telemetry::Redacted::new(SIGNING_KEY.into()),
        inbound_secret: sanvi_telemetry::Redacted::new(INBOUND_KEY.into()),
    };
    let closed_provider = RemotePaymentsPrivacyProvider::new(&closed_config).unwrap();

    let collect_err = closed_provider.collect(&subject).await;
    assert!(
        collect_err.is_err(),
        "collect against unreachable service must fail, got: {collect_err:?}"
    );

    let erase_err = closed_provider.erase(&subject, EraseMode::Full).await;
    assert!(
        erase_err.is_err(),
        "erase against unreachable service must fail, got: {erase_err:?}"
    );
}
