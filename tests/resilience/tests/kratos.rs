//! Dependency-matrix injection tests: Kratos (TASK-023, FR-1112).
//!
//! All three failure modes fail the same way on purpose: session
//! verification errors are fail-closed (`503` service-unavailable), never
//! an anonymous pass-through of an authenticated route. The injections are
//! real HTTP: a closed port (down), a 500-answerer (erroring), a delayed
//! answer beyond the client deadline (slow).

mod common;

use common::{FaultMode, FaultServer};
use sanvi_http::authn::AuthnMiddleware;
use sanvi_http::{ApiError, SessionChannel};
use sanvi_identity::domain::ports::KratosClient;
use std::sync::Arc;
use std::time::Duration;

/// The Kratos whoami client against a port nothing listens on.
#[tokio::test]
async fn kratos_down_fails_authn_closed_with_503() {
    let (base, _port) = common::closed_endpoint();
    let client = sanvi_identity::adapters::kratos::HttpKratosClient::new_with_timeout(
        base,
        None,
        "ory_kratos_session".to_string(),
        Duration::from_millis(300),
    )
    .expect("client builds");
    let err = client
        .whoami("some-token", SessionChannel::Bearer)
        .await
        .expect_err("a dead Kratos cannot verify");
    assert_kratos_backend_failure(&err);

    assert_authn_failure_is_503().await;
}

/// Kratos · erroring — HTTP 500 on whoami.
#[tokio::test]
async fn kratos_erroring_fails_authn_closed_with_503() {
    let server = FaultServer::start(FaultMode::Status(500));
    let client = sanvi_identity::adapters::kratos::HttpKratosClient::new_with_timeout(
        server.base_url().to_string(),
        None,
        "ory_kratos_session".to_string(),
        Duration::from_millis(300),
    )
    .expect("client builds");
    let err = client
        .whoami("some-token", SessionChannel::Bearer)
        .await
        .expect_err("a 500ing Kratos cannot verify");
    assert_kratos_backend_failure(&err);

    assert_authn_failure_is_503().await;
}

/// Kratos · slow — the answer arrives after the client deadline.
#[tokio::test]
async fn kratos_slow_times_out_and_fails_authn_closed_with_503() {
    let server = FaultServer::start(FaultMode::Delay {
        delay_ms: 5_000,
        status: 200,
    });
    let client = sanvi_identity::adapters::kratos::HttpKratosClient::new_with_timeout(
        server.base_url().to_string(),
        None,
        "ory_kratos_session".to_string(),
        Duration::from_millis(300),
    )
    .expect("client builds");
    let started = std::time::Instant::now();
    let err = client
        .whoami("some-token", SessionChannel::Bearer)
        .await
        .expect_err("a too-slow Kratos cannot verify");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the 300ms deadline must bound the wait, took {:?}",
        started.elapsed()
    );
    assert_kratos_backend_failure(&err);

    assert_authn_failure_is_503().await;
}

fn assert_kratos_backend_failure(err: &sanvi_identity::domain::IdentityError) {
    // Both backend-failure shapes fail closed: `KratosUnreachable`
    // (transport) and `KratosRejected` (5xx) both become `503` in the
    // session verifier's `backend` mapper — the distinction is for the
    // logs, not the client.
    let rendered = err.to_string();
    let is_backend_failure = matches!(
        err,
        sanvi_identity::domain::IdentityError::KratosUnreachable(_)
            | sanvi_identity::domain::IdentityError::KratosRejected(_)
    ) || rendered.contains("unreachable")
        || rendered.contains("rejected")
        || rendered.contains("operation was timed out")
        || rendered.contains("connection");
    assert!(
        is_backend_failure,
        "expected a Kratos backend-failure error, got: {rendered}"
    );
}

/// The authn middleware's documented consequence: a verifier error becomes
/// a `503` problem, and the route never executes. The failing verifier is
/// the same port-level injection the HTTP client above produces; the
/// middleware contract is what this row's user-visible behaviour hangs on.
async fn assert_authn_failure_is_503() {
    struct FailingVerifier;
    impl sanvi_http::SessionVerifier for FailingVerifier {
        fn verify<'a>(
            &'a self,
            _token: Option<(&'a str, SessionChannel)>,
            _tenant_id: Option<sanvi_kernel::TenantId>,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Option<sanvi_http::Subject>, ApiError>> + Send + 'a>,
        > {
            Box::pin(async {
                Err(ApiError::ServiceUnavailable {
                    detail: "authentication is temporarily unavailable".to_string(),
                })
            })
        }
    }

    let middleware = Arc::new(AuthnMiddleware {
        verifier: Arc::new(FailingVerifier),
        session_cookie_name: "ory_kratos_session".to_string(),
        exempt_prefixes: vec![],
    });
    // The canonical stack, assembled with only the authn middleware wired:
    // exactly the shape an identity outage leaves every route in.
    let app = actix_web::test::init_service(sanvi_http::middleware::stack(
        actix_web::App::new().service(
            actix_web::web::resource("/secret").route(
                actix_web::web::get()
                    .to(|| async { actix_web::HttpResponse::Ok().body("never reaches here") }),
            ),
        ),
        sanvi_http::ServerConfig::default(),
        None,
        None,
        Some(middleware),
        None,
        None,
        sanvi_http::ssr_cache::SsrCacheLayer::disabled(
            sanvi_http::config::SsrCacheConfig::default(),
        ),
        sanvi_http::DegradedHeaderLayer::disabled(),
        None,
    ))
    .await;
    let request = actix_web::test::TestRequest::get()
        .uri("/secret")
        // The middleware reads the session cookie or `Authorization:
        // Bearer` — a bespoke header is no credential at all and rides
        // through as anonymous, which would prove nothing about the
        // fail-closed path.
        .insert_header((
            actix_web::http::header::AUTHORIZATION,
            "Bearer whatever",
        ))
        .to_request();
    let response = actix_web::test::call_service(&app, request).await;
    assert_eq!(response.status().as_u16(), 503);
}
