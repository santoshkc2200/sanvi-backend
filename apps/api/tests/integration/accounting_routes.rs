//! Tenant accounting routes against the assembled app and a real Postgres.
//!
//! Kratos and Keto are stubs: each session token names one persona and
//! whether its AAL2 authentication is fresh. Keto confirms every tuple, so
//! the authoritative tenant membership (`identity.membership_roles`) and
//! the seeded system roles decide each permission check, exactly as in
//! production. freee is replaced by a `ProviderOAuth` double; nothing here
//! reaches the network.

use std::collections::BTreeMap;
use std::sync::Arc;

use actix_web::test as aw_test;
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};

use sanvi_accounting::{
    AccountingConfig,
    adapters::{
        PostgresAccountingStore, PostgresCredentialStore, UnavailableVerifier,
        http::{AccountingState, Consent},
    },
    application::{
        ActivateBook, Authorization, CancelSwitch, CompleteAuthorization, CreateBook, GetBook,
        ListBooks, ProviderOAuth, RegisterConnection, RevokeAuthorization, ScheduleSwitch,
        Services, StartAuthorization,
    },
    domain::{AccountingError, Provider, Secret, TokenGrant},
};
use sanvi_api::{
    AppConfig, IdentityDependencies, PlatformAdminDependencies, TenantCatalogAdapter,
    accounting::{AuthorizerAccess, build_accounting_state},
    build_access_state, build_app, build_audit_state, build_identity_state,
    build_platform_admin_state, build_system_state, build_tenancy_state,
};
use sanvi_persistence::{PgPool, TxManager};
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

const ORGANIZATION: &str = "12345";
const ACCESS_TOKEN: &str = "freee-access-token-7c1e";
const REFRESH_TOKEN: &str = "freee-refresh-token-4b9d";
const GOOD_CODE: &str = "good-code";

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

/// freee's OAuth endpoints: one good code, limited to `ORGANIZATION`.
struct FakeFreee;

#[async_trait]
impl ProviderOAuth for FakeFreee {
    fn provider(&self) -> Provider {
        Provider::Freee
    }
    fn authorization_url(&self, state: &str) -> String {
        format!("https://freee.test/authorize?state={state}")
    }
    async fn exchange_code(&self, code: &str) -> Result<TokenGrant, AccountingError> {
        if code != GOOD_CODE {
            return Err(AccountingError::AuthorizationRejected("code rejected"));
        }
        Ok(TokenGrant {
            access_token: Secret::new(ACCESS_TOKEN.to_string()),
            refresh_token: Secret::new(REFRESH_TOKEN.to_string()),
            access_expires_at: Utc::now() + TimeDelta::hours(6),
            organization_id: Some(ORGANIZATION.to_string()),
        })
    }
    async fn refresh(&self, _: &Secret) -> Result<TokenGrant, AccountingError> {
        Err(AccountingError::ReauthorizationRequired)
    }
    async fn revoke(&self, _: &Secret) -> Result<(), AccountingError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// How the accounting context is composed for one test.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Composition {
    /// `features.accounting` off: no state, no routes.
    Off,
    /// Flag on, freee secrets empty: the production fail-closed wiring.
    WithoutSecrets,
    /// Flag on, consent backed by [`FakeFreee`]; verification still
    /// unavailable, so activation answers 503.
    WithConsent,
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

fn accounting_state(
    composition: Composition,
    pool: &PgPool,
    authorizer: Arc<dyn sanvi_access::domain::Authorizer>,
) -> Option<AccountingState> {
    let ids = sanvi_testsupport::test_ids();
    let transactions = TxManager::new(pool.clone(), ids.clone());
    let clock: Arc<dyn sanvi_kernel::Clock> = Arc::new(sanvi_kernel::SystemClock);
    match composition {
        Composition::Off => None,
        Composition::WithoutSecrets => Some(
            build_accounting_state(
                &AccountingConfig::default(),
                transactions,
                ids,
                authorizer,
                clock,
            )
            .expect("empty secrets compose"),
        ),
        Composition::WithConsent => {
            let services = Arc::new(Services {
                store: Arc::new(PostgresAccountingStore::new(
                    transactions.clone(),
                    ids.clone(),
                )),
                access: Arc::new(AuthorizerAccess(authorizer)),
                verifier: Arc::new(UnavailableVerifier),
                clock,
                ids: ids.clone(),
            });
            let authorization = Arc::new(Authorization {
                services: services.clone(),
                credentials: Arc::new(PostgresCredentialStore::new(
                    transactions,
                    ids,
                    [7u8; 32],
                    "test-kek".to_string(),
                )),
                providers: vec![Arc::new(FakeFreee)],
                state_key: Arc::from(&b"accounting-routes-state-secret-0123"[..]),
            });
            Some(AccountingState {
                list_books: Arc::new(ListBooks(services.clone())),
                get_book: Arc::new(GetBook(services.clone())),
                create_book: Arc::new(CreateBook(services.clone())),
                register_connection: Arc::new(RegisterConnection(services.clone())),
                activate_book: Arc::new(ActivateBook(services.clone())),
                schedule_switch: Arc::new(ScheduleSwitch(services.clone())),
                cancel_switch: Arc::new(CancelSwitch(services)),
                consent: Some(Consent {
                    start: Arc::new(StartAuthorization(authorization.clone())),
                    complete: Arc::new(CompleteAuthorization(authorization.clone())),
                    revoke: Arc::new(RevokeAuthorization(authorization)),
                }),
            })
        }
    }
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
    let config = config(kratos, keto, composition);
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
    let accounting = accounting_state(composition, &pool, access.authorizer.clone());

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
        None, // payments
        None, // advertising
        accounting,
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

/// Removes (or restores) the owner's `tenant_owner` role in `TENANT`.
async fn set_owner_role(pool: &PgPool, granted: bool) {
    let role = system_role(pool, "tenant_owner").await;
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(TENANT).await.unwrap();
    let statement = if granted {
        "INSERT INTO identity.membership_roles (tenant_id, user_id, role_id) VALUES ($1, $2, $3)"
    } else {
        "DELETE FROM identity.membership_roles WHERE tenant_id = $1 AND user_id = $2 AND role_id = $3"
    };
    sqlx::query(statement)
        .bind(TENANT)
        .bind(OWNER)
        .bind(role)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
    scope.commit().await.unwrap();
}

async fn unredeemed_states(pool: &PgPool) -> i64 {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(TENANT).await.unwrap();
    sqlx::query_scalar(
        "SELECT count(*) FROM accounting.oauth_states WHERE tenant_id = $1 AND redeemed_at IS NULL",
    )
    .bind(TENANT)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

struct Reply {
    status: u16,
    etag: Option<String>,
    headers: String,
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
    if_match: Option<String>,
    body: Option<Value>,
}

impl<'a> Request<'a> {
    fn new(method: &'a str, uri: impl Into<String>) -> Self {
        Self {
            method,
            uri: uri.into(),
            persona: &OWNER_FRESH,
            host: HOST,
            if_match: None,
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
    fn if_match(mut self, revision: &str) -> Self {
        self.if_match = Some(revision.to_string());
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
        if let Some(revision) = self.if_match {
            builder = builder.insert_header((actix_web::http::header::IF_MATCH, revision));
        }
        if let Some(body) = self.body {
            builder = builder.set_json(body);
        }
        let response = aw_test::call_service(app, builder.to_request()).await;
        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get(actix_web::http::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let headers = format!("{:?}", response.headers());
        let bytes = actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap();
        Reply {
            status,
            etag,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }
}

const BOOKS: &str = "/api/v1/tenant/accounting/books";
const CALLBACK: &str = "/api/v1/tenant/accounting/authorization/callback";

fn connection_body() -> Value {
    json!({
        "provider": "freee",
        "organization_id": ORGANIZATION,
        "mappings": {
            "revenue_account": "売上高",
            "refund_account": "売上値引",
            "clearing_account": "売掛金",
            "tax_codes": {"standard": "21"}
        }
    })
}

/// Creates a book and registers one connection; returns the book ID, the
/// connection ID and the current `ETag`.
async fn book_with_connection<S, B>(app: &S) -> (String, String, String)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody + 'static,
    B::Error: std::fmt::Debug,
{
    let created = Request::new("POST", BOOKS)
        .json(json!({"legal_entity_key": "main"}))
        .send(app)
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let book_id = created.json()["id"].as_str().unwrap().to_string();
    let registered = Request::new("POST", format!("{BOOKS}/{book_id}/connections"))
        .if_match(created.etag.as_deref().unwrap())
        .json(connection_body())
        .send(app)
        .await;
    assert_eq!(registered.status, 200, "{}", registered.body);
    let connection_id = registered.json()["connections"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    (book_id, connection_id, registered.etag.unwrap())
}

fn state_from(authorization_url: &str) -> String {
    authorization_url
        .split_once("state=")
        .unwrap()
        .1
        .to_string()
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
async fn entitlement_is_required() {
    let (_db, app) = app!(Composition::WithoutSecrets);
    let reply = Request::new("GET", BOOKS)
        .host(UNENTITLED_HOST)
        .send(&app)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    let entitled = Request::new("GET", BOOKS).send(&app).await;
    assert_eq!(entitled.status, 200, "{}", entitled.body);
}

#[actix_web::test]
async fn member_cannot_read_owner_and_admin_can() {
    let (_db, app) = app!(Composition::WithoutSecrets);
    let member = Request::new("GET", BOOKS)
        .persona(&MEMBER_FRESH)
        .send(&app)
        .await;
    assert_eq!(member.status, 403, "{}", member.body);
    for persona in [&OWNER_FRESH, &ADMIN_FRESH] {
        let reply = Request::new("GET", BOOKS).persona(persona).send(&app).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
    }
    let member_create = Request::new("POST", BOOKS)
        .persona(&MEMBER_FRESH)
        .json(json!({"legal_entity_key": "main"}))
        .send(&app)
        .await;
    assert_eq!(member_create.status, 403, "{}", member_create.body);
    let admin_create = Request::new("POST", BOOKS)
        .persona(&ADMIN_FRESH)
        .json(json!({"legal_entity_key": "main"}))
        .send(&app)
        .await;
    assert_eq!(admin_create.status, 201, "{}", admin_create.body);
}

#[actix_web::test]
async fn create_register_and_read_round_trip() {
    let (_db, app) = app!(Composition::WithoutSecrets);
    let created = Request::new("POST", BOOKS)
        .json(json!({"legal_entity_key": "main"}))
        .send(&app)
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let book = created.json();
    let revision = book["revision"].as_i64().unwrap();
    assert_eq!(created.etag, Some(format!("\"{revision}\"")));
    let book_id = book["id"].as_str().unwrap();

    let registered = Request::new("POST", format!("{BOOKS}/{book_id}/connections"))
        .if_match(created.etag.as_deref().unwrap())
        .json(connection_body())
        .send(&app)
        .await;
    assert_eq!(registered.status, 200, "{}", registered.body);
    assert_eq!(
        registered.etag,
        Some(format!("\"{}\"", revision + 1)),
        "registering bumps the revision"
    );

    let read = Request::new("GET", format!("{BOOKS}/{book_id}"))
        .send(&app)
        .await;
    assert_eq!(read.status, 200, "{}", read.body);
    assert_eq!(read.etag, registered.etag);
    let connection = &read.json()["connections"][0];
    assert_eq!(connection["provider"], "freee");
    assert_eq!(connection["organization_id"], ORGANIZATION);
    assert_eq!(connection["credential_status"], "none");

    let listed = Request::new("GET", BOOKS).send(&app).await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(listed.json()[0]["id"], book_id);
}

#[actix_web::test]
async fn missing_if_match_is_400_and_stale_is_409() {
    let (_db, app) = app!(Composition::WithoutSecrets);
    let created = Request::new("POST", BOOKS)
        .json(json!({"legal_entity_key": "main"}))
        .send(&app)
        .await;
    let book_id = created.json()["id"].as_str().unwrap().to_string();
    let uri = format!("{BOOKS}/{book_id}/connections");

    let missing = Request::new("POST", uri.clone())
        .json(connection_body())
        .send(&app)
        .await;
    assert_eq!(missing.status, 400, "{}", missing.body);
    let malformed = Request::new("POST", uri.clone())
        .if_match("\"one\"")
        .json(connection_body())
        .send(&app)
        .await;
    assert_eq!(malformed.status, 400, "{}", malformed.body);
    let stale = Request::new("POST", uri)
        .if_match("\"999\"")
        .json(connection_body())
        .send(&app)
        .await;
    assert_eq!(stale.status, 409, "{}", stale.body);
}

#[actix_web::test]
async fn fresh_mfa_is_required_for_consent_activation_and_switch() {
    let (_db, app) = app!(Composition::WithConsent);
    let (book_id, connection_id, etag) = book_with_connection(&app).await;
    let authorization = format!("{BOOKS}/{book_id}/connections/{connection_id}/authorization");
    let at = json!({"connection_id": connection_id, "starts_at": "2030-01-01T00:00:00Z"});
    let switch = json!({"connection_id": connection_id, "cutover_at": "2030-01-01T00:00:00Z"});
    let routes = [
        ("POST", authorization.clone(), None),
        ("DELETE", authorization, None),
        ("POST", format!("{BOOKS}/{book_id}/activation"), Some(at)),
        ("PUT", format!("{BOOKS}/{book_id}/switch"), Some(switch)),
    ];
    for (method, uri, body) in &routes {
        let mut request = Request::new(method, uri.clone())
            .persona(&OWNER_STALE)
            .if_match(&etag);
        if let Some(body) = body {
            request = request.json(body.clone());
        }
        let reply = request.send(&app).await;
        assert_eq!(reply.status, 403, "{method} {uri}: {}", reply.body);
    }

    // With fresh MFA the same requests pass the guard: start issues a URL,
    // and activation reaches verification, which is unavailable here.
    let start = Request::new("POST", routes[0].1.clone()).send(&app).await;
    assert_eq!(start.status, 200, "{}", start.body);
    let activation = Request::new("POST", routes[2].1.clone())
        .if_match(&etag)
        .json(routes[2].2.clone().unwrap())
        .send(&app)
        .await;
    assert_eq!(activation.status, 503, "{}", activation.body);
}

#[actix_web::test]
async fn consent_routes_answer_503_without_secrets() {
    let (_db, app) = app!(Composition::WithoutSecrets);
    let (book_id, connection_id, _) = book_with_connection(&app).await;
    let authorization = format!("{BOOKS}/{book_id}/connections/{connection_id}/authorization");
    for (method, uri, body) in [
        ("POST", authorization.clone(), None),
        ("DELETE", authorization, None),
        (
            "POST",
            CALLBACK.to_string(),
            Some(json!({"state": "s", "code": GOOD_CODE})),
        ),
    ] {
        let mut request = Request::new(method, uri.clone());
        if let Some(body) = body {
            request = request.json(body);
        }
        let reply = request.send(&app).await;
        assert_eq!(reply.status, 503, "{method} {uri}: {}", reply.body);
        assert!(
            reply.json()["type"]
                .as_str()
                .is_some_and(|kind| kind.ends_with("accounting/provider-unavailable")),
            "{}",
            reply.body
        );
    }
}

#[actix_web::test]
async fn activation_answers_503_without_secrets() {
    let (_db, app) = app!(Composition::WithoutSecrets);
    let (book_id, connection_id, etag) = book_with_connection(&app).await;
    let reply = Request::new("POST", format!("{BOOKS}/{book_id}/activation"))
        .if_match(&etag)
        .json(json!({"connection_id": connection_id, "starts_at": "2030-01-01T00:00:00Z"}))
        .send(&app)
        .await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    let unchanged = Request::new("GET", format!("{BOOKS}/{book_id}"))
        .send(&app)
        .await;
    assert_eq!(unchanged.etag.as_deref(), Some(etag.as_str()));
}

#[actix_web::test]
async fn callback_after_losing_manage_is_403_and_keeps_state() {
    let (db, app) = app!(Composition::WithConsent);
    let pool = db.app_pool();
    let (book_id, connection_id, _) = book_with_connection(&app).await;
    let start = Request::new(
        "POST",
        format!("{BOOKS}/{book_id}/connections/{connection_id}/authorization"),
    )
    .send(&app)
    .await;
    assert_eq!(start.status, 200, "{}", start.body);
    let state = state_from(start.json()["authorization_url"].as_str().unwrap());

    set_owner_role(&pool, false).await;
    let denied = Request::new("POST", CALLBACK)
        .json(json!({"state": state, "code": "wrong-code"}))
        .send(&app)
        .await;
    assert_eq!(denied.status, 403, "{}", denied.body);
    assert_eq!(
        unredeemed_states(&pool).await,
        1,
        "a denied callback burned the state"
    );

    // Back in role, the same state reaches the code exchange: the double's
    // rejection proves the state was still unused.
    set_owner_role(&pool, true).await;
    let exchanged = Request::new("POST", CALLBACK)
        .json(json!({"state": state, "code": "wrong-code"}))
        .send(&app)
        .await;
    assert_eq!(exchanged.status, 400, "{}", exchanged.body);
    assert!(
        exchanged.json()["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("accounting/authorization-rejected")),
        "{}",
        exchanged.body
    );
}

#[actix_web::test]
async fn naive_datetime_is_rejected() {
    let (_db, app) = app!(Composition::WithoutSecrets);
    let (book_id, connection_id, etag) = book_with_connection(&app).await;
    let reply = Request::new("POST", format!("{BOOKS}/{book_id}/activation"))
        .if_match(&etag)
        .json(json!({"connection_id": connection_id, "starts_at": "2026-11-01T00:00:00"}))
        .send(&app)
        .await;
    assert_eq!(reply.status, 400, "{}", reply.body);
}

#[actix_web::test]
async fn unknown_body_field_is_rejected() {
    let (_db, app) = app!(Composition::WithoutSecrets);
    let reply = Request::new("POST", BOOKS)
        .json(json!({"legal_entity_key": "main", "tenant_id": TENANT.to_string()}))
        .send(&app)
        .await;
    assert_eq!(reply.status, 400, "{}", reply.body);
}

#[actix_web::test]
async fn responses_never_contain_tokens() {
    let (_db, app) = app!(Composition::WithConsent);
    let (book_id, connection_id, _) = book_with_connection(&app).await;
    let start = Request::new(
        "POST",
        format!("{BOOKS}/{book_id}/connections/{connection_id}/authorization"),
    )
    .send(&app)
    .await;
    assert_eq!(start.status, 200, "{}", start.body);
    let state = state_from(start.json()["authorization_url"].as_str().unwrap());

    let mut replies = vec![
        Request::new("POST", CALLBACK)
            .json(json!({"state": state, "code": GOOD_CODE}))
            .send(&app)
            .await,
    ];
    assert_eq!(replies[0].status, 200, "{}", replies[0].body);
    assert_eq!(
        replies[0].json()["connections"][0]["credential_status"],
        "active"
    );
    replies.push(
        Request::new("GET", format!("{BOOKS}/{book_id}"))
            .send(&app)
            .await,
    );
    replies.push(Request::new("GET", BOOKS).send(&app).await);
    replies.push(
        Request::new("POST", CALLBACK)
            .json(json!({"state": state, "code": GOOD_CODE}))
            .send(&app)
            .await,
    );
    assert_eq!(replies[3].status, 400, "a replayed state is rejected");

    for reply in &replies {
        for secret in [ACCESS_TOKEN, REFRESH_TOKEN, GOOD_CODE, state.as_str()] {
            assert!(
                !reply.body.contains(secret) && !reply.headers.contains(secret),
                "a response leaked credential material: {} {}",
                reply.headers,
                reply.body
            );
        }
    }
}
