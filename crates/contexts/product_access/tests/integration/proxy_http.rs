use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use actix_web::dev::Service;
use actix_web::{App, HttpMessage, HttpRequest, HttpResponse, HttpServer, test, web};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sanvi_http::{ResolutionSource, Subject, TenantContext, TenantRuntimeStatus};
use sanvi_kernel::{RoleId, TenantId, UserId};
use sanvi_product_access::adapters::http::ProductAccessState;
use sanvi_product_access::adapters::{Ed25519Signer, HttpProductUpstream};
use sanvi_product_access::application::ProxyRequest;
use sanvi_product_access::domain::{
    ECOMMERCE, EntitlementView, ProductAccessError, RolePermissions, TenantEntitlements,
};
use sanvi_product_access::routes::product_access_tenant_scope_routes;

const SERVICE_KEY: &str = "test-service-key-0001";
const SEED: &str = "0909090909090909090909090909090909090909090909090909090909090909";

/// What the stand-in upstream received.
#[derive(Debug, Clone)]
struct Received {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: String,
}

async fn upstream_handler(
    request: HttpRequest,
    body: web::Bytes,
    log: web::Data<Mutex<Vec<Received>>>,
) -> HttpResponse {
    log.lock().unwrap().push(Received {
        method: request.method().to_string(),
        uri: request.uri().to_string(),
        headers: request
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .collect(),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    match request.path() {
        "/v1/unauthorized" => HttpResponse::Unauthorized().finish(),
        "/v1/missing" => HttpResponse::NotFound()
            .content_type("application/problem+json")
            .body(r#"{"type":"https://sanvi.dev/problems/not-found","status":404}"#),
        _ => HttpResponse::Created()
            .content_type("application/json")
            .insert_header(("Idempotency-Replayed", "true"))
            .insert_header(("Set-Cookie", "upstream=leak"))
            .insert_header(("X-Internal", "leak"))
            .body(r#"{"id":"1"}"#),
    }
}

async fn start_upstream() -> (SocketAddr, Arc<Mutex<Vec<Received>>>) {
    let log = web::Data::new(Mutex::new(Vec::new()));
    let shared = log.clone().into_inner();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(log.clone())
            .default_service(web::to(upstream_handler))
    })
    .workers(1)
    .bind(("127.0.0.1", 0))
    .unwrap();
    let address = server.addrs()[0];
    actix_web::rt::spawn(server.run());
    (address, shared)
}

struct Grants {
    enabled: bool,
    permissions: Vec<String>,
}

#[async_trait]
impl TenantEntitlements for Grants {
    async fn resolve(&self, _: TenantId) -> Result<Vec<EntitlementView>, ProductAccessError> {
        Ok(vec![
            EntitlementView {
                key: "products.ecommerce.enabled".into(),
                enabled: self.enabled,
                limit: None,
            },
            EntitlementView {
                key: "ecommerce.stores.max".into(),
                enabled: true,
                limit: Some(3),
            },
        ])
    }
}

#[async_trait]
impl RolePermissions for Grants {
    async fn permissions(
        &self,
        _: TenantId,
        _: &[RoleId],
    ) -> Result<Vec<String>, ProductAccessError> {
        Ok(self.permissions.clone())
    }
}

fn tenant() -> TenantContext {
    TenantContext {
        tenant_id: TenantId::try_from(1001_i64).unwrap(),
        slug: "acme".into(),
        display_name: "Acme".into(),
        status: TenantRuntimeStatus::Active,
        region: "jp".into(),
        default_locale: "ja".into(),
        resolution_source: ResolutionSource::Subdomain,
    }
}

fn subject() -> Subject {
    Subject {
        user_id: UserId::try_from(3001_i64).unwrap(),
        session_id: "s".into(),
        aal: "aal1".into(),
        methods: vec!["password".into()],
        tenant_id: Some(TenantId::try_from(1001_i64).unwrap()),
        role_ids: vec![RoleId::try_from(7_i64).unwrap()],
        authenticated_at: None,
    }
}

fn state(base_url: &str, grants: Grants) -> ProductAccessState {
    let grants = Arc::new(grants);
    ProductAccessState {
        ecommerce: Arc::new(ProxyRequest::new(
            grants.clone(),
            grants,
            Arc::new(Ed25519Signer::from_seed_hex("test-kid", SEED).unwrap()),
            Arc::new(HttpProductUpstream::new(&ECOMMERCE, base_url, SERVICE_KEY).unwrap()),
        )),
    }
}

/// The app as the platform mounts it, with tenancy and authentication
/// already resolved by the (here simulated) middleware.
macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($state))
                .wrap_fn(|request, service| {
                    request.extensions_mut().insert(tenant());
                    request.extensions_mut().insert(subject());
                    service.call(request)
                })
                .service(
                    web::scope("/api/v1/tenant").configure(product_access_tenant_scope_routes),
                ),
        )
        .await
    };
}

fn manager() -> Grants {
    Grants {
        enabled: true,
        permissions: vec![
            "ecommerce.store.manage".into(),
            "ecommerce.store.read".into(),
            "media.assets.manage".into(),
        ],
    }
}

fn claims(token: &str) -> serde_json::Value {
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

#[actix_web::test]
async fn forwards_with_platform_credentials_and_nothing_of_the_browsers() {
    let (address, log) = start_upstream().await;
    let app = app!(state(&format!("http://{address}"), manager()));

    let request = test::TestRequest::post()
        .uri("/api/v1/tenant/ecommerce/stores?limit=2")
        .insert_header(("Content-Type", "application/json"))
        .insert_header(("Idempotency-Key", "k-1"))
        .insert_header(("Cookie", "ory_kratos_session=browser-secret"))
        .insert_header(("Authorization", "Bearer browser-token"))
        .insert_header(("X-Tenant-Id", "999"))
        .insert_header(("X-Identity-Token", "forged"))
        .set_payload(r#"{"name":"Tokyo"}"#)
        .to_request();
    let response = test::call_service(&app, request).await;

    assert_eq!(response.status(), 201);
    let headers = response.headers();
    assert_eq!(headers.get("idempotency-replayed").unwrap(), "true");
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    assert!(headers.get("set-cookie").is_none());
    assert!(headers.get("x-internal").is_none());
    assert_eq!(test::read_body(response).await, r#"{"id":"1"}"#);

    let received = log.lock().unwrap()[0].clone();
    assert_eq!(received.method, "POST");
    assert_eq!(received.uri, "/v1/stores?limit=2");
    assert_eq!(received.body, r#"{"name":"Tokyo"}"#);
    let header = |name: &str| {
        received
            .headers
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(header("authorization"), [format!("Bearer {SERVICE_KEY}")]);
    assert_eq!(header("idempotency-key"), ["k-1"]);
    assert!(header("cookie").is_empty());
    assert!(header("x-tenant-id").is_empty());

    let tokens = header("x-identity-token");
    assert_eq!(tokens.len(), 1);
    assert_ne!(tokens[0], "forged");
    let claims = claims(&tokens[0]);
    assert_eq!(claims["iss"], "sanvi-backend");
    assert_eq!(claims["aud"], "ecommerce-service");
    assert_eq!(claims["tenant_id"], "1001");
    assert_eq!(claims["sub"], "3001");
    assert_eq!(
        claims["permissions"],
        serde_json::json!(["ecommerce.store.manage", "ecommerce.store.read"])
    );
    assert_eq!(claims["limits"]["ecommerce.stores.max"], 3);
}

#[actix_web::test]
async fn product_problems_pass_through_and_credential_rejections_do_not() {
    let (address, _) = start_upstream().await;
    let app = app!(state(&format!("http://{address}"), manager()));

    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/ecommerce/missing")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );

    // A 401 from upstream means the platform's own key is wrong; the
    // browser must not read it as its session expiring.
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/v1/tenant/ecommerce/unauthorized")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 502);
}

#[actix_web::test]
async fn refused_requests_never_reach_the_product() {
    let (address, log) = start_upstream().await;
    let base = format!("http://{address}");
    let cases = [
        (
            Grants {
                enabled: false,
                ..manager()
            },
            "stores",
            403,
        ),
        (
            Grants {
                enabled: true,
                permissions: vec!["media.assets.manage".into()],
            },
            "stores",
            403,
        ),
        (manager(), "stores/../health", 400),
        (manager(), "stores/%2e%2e/health", 400),
    ];
    for (grants, path, status) in cases {
        let app = app!(state(&base, grants));
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/api/v1/tenant/ecommerce/{path}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), status, "{path}");
    }
    assert!(log.lock().unwrap().is_empty());
}

#[actix_web::test]
async fn unreachable_or_unconfigured_products_answer_503() {
    let unreachable = state("http://127.0.0.1:1", manager());
    let unconfigured = state("", manager());
    for state in [unreachable, unconfigured] {
        let app = app!(state);
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/tenant/ecommerce/stores")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 503);
    }
}
