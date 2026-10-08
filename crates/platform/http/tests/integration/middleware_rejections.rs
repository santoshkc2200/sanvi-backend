//! Contract tests for the middleware stack's early-rejection paths.
//!
//! No Redis/Postgres needed: these pin the fail-closed, timeout, panic and
//! concurrency behaviors of the protective layers.

use std::{sync::Arc, time::Duration};

use actix_web::{App, body::MessageBody, http::StatusCode, test, web};
use sanvi_http::{ServerConfig, bind_without_run, middleware};

/// Run the app on a real socket (ephemeral port) so that middleware errors
/// flow through the same `Error -> Response` conversion as production.
/// Returns the base URL and the server task.
async fn spawn_real<F, I, S, B>(
    app_factory: F,
) -> (String, tokio::task::JoinHandle<std::io::Result<()>>)
where
    F: Fn() -> I + Send + Clone + 'static,
    I: actix_service::IntoServiceFactory<S, actix_http::Request>,
    S: actix_web::dev::ServiceFactory<
            actix_http::Request,
            Config = actix_web::dev::AppConfig,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
            InitError = (),
        > + 'static,
    B: MessageBody + 'static,
{
    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        workers: 1, // per-worker state (e.g. concurrency semaphores) must be observable
        ..ServerConfig::default()
    };
    let server = bind_without_run(&config, app_factory).expect("bind");
    let addr = server.addrs().first().expect("bound addr").to_string();
    let task = tokio::spawn(server.run());
    (format!("http://{addr}"), task)
}

/// Minimal HTTP GET over a raw socket (no extra dependencies).
async fn raw_get(url: String) -> (u16, Vec<(String, String)>, String) {
    let rest = url.strip_prefix("http://").unwrap();
    let (authority, path_part) = rest.split_once('/').unwrap_or((rest, ""));
    let path = format!("/{path_part}");
    let (host, port) = authority.split_once(':').unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect((host, port.parse::<u16>().unwrap()))
        .await
        .expect("connect");
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1
Host: {host}
Connection: close

"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers: Vec<(String, String)> = text
        .lines()
        .skip(1)
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let body = text
        .split(
            "

",
        )
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (status, headers, body)
}

fn test_config() -> ServerConfig {
    let mut config = ServerConfig::default();
    config.rate_limit.enabled = true;
    config.rate_limit.fail_open = false; // no Redis attached → 503
    config.request_timeout_secs = 1;
    config.max_concurrency = 1000;
    config
}

fn build_app(
    config: ServerConfig,
) -> App<
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
    let app = App::new().route("/ok", web::get().to(|| async { "ok" }));
    middleware::stack(
        app,
        config,
        None,
        None,
        None,
        None,
        None,
        sanvi_http::ssr_cache::SsrCacheLayer::disabled(
            sanvi_http::config::SsrCacheConfig::default(),
        ),
        sanvi_http::degraded::DegradedHeaderLayer::disabled(),
        None,
    )
}

#[actix_web::test]
async fn rate_limit_fails_closed_when_backend_is_missing() {
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get().uri("/ok").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[actix_web::test]
async fn rate_limit_is_skipped_when_disabled() {
    let mut config = test_config();
    config.rate_limit.enabled = false;
    let app = test::init_service(build_app(config)).await;
    let req = test::TestRequest::get().uri("/ok").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix_web::test]
async fn rate_limit_exempts_health_paths() {
    let app = test::init_service(build_app(test_config())).await;
    // /healthz is exempt even though no handler is registered for it here:
    // the rate limiter must not be the thing that rejects it.
    let req = test::TestRequest::get().uri("/healthz").to_request();
    let res = test::call_service(&app, req).await;
    assert_ne!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[actix_web::test]
async fn slow_handler_times_out_with_504() {
    // Rate limiting is off so only the timeout middleware can reject.
    let mut config = test_config();
    config.request_timeout_secs = 1;
    config.rate_limit.enabled = false;
    let (url, task) = spawn_real(move || {
        let app = App::new().route(
            "/slow",
            web::get().to(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "never"
            }),
        );
        middleware::stack(
            app,
            config.clone(),
            None,
            None,
            None,
            None,
            None,
            sanvi_http::ssr_cache::SsrCacheLayer::disabled(
                sanvi_http::config::SsrCacheConfig::default(),
            ),
            sanvi_http::degraded::DegradedHeaderLayer::disabled(),
            None,
        )
    })
    .await;
    let (status, _, body) = raw_get(format!("{url}/slow")).await;
    assert_eq!(status, 504, "body: {body}");
    task.abort();
}

#[actix_web::test]
async fn panicking_handler_becomes_500() {
    async fn boom() -> &'static str {
        panic!("kaboom")
    }
    let mut config = test_config();
    config.rate_limit.enabled = false;
    let (url, task) = spawn_real(move || {
        let app = App::new().route("/boom", web::get().to(boom));
        middleware::stack(
            app,
            config.clone(),
            None,
            None,
            None,
            None,
            None,
            sanvi_http::ssr_cache::SsrCacheLayer::disabled(
                sanvi_http::config::SsrCacheConfig::default(),
            ),
            sanvi_http::degraded::DegradedHeaderLayer::disabled(),
            None,
        )
    })
    .await;
    let (status, _, body) = raw_get(format!("{url}/boom")).await;
    assert_eq!(status, 500, "body: {body}");
    // Panic details must not leak.
    assert!(!body.contains("kaboom"));
    task.abort();
}

#[actix_web::test]
async fn concurrency_limit_rejects_when_saturated() {
    let mut config = test_config();
    config.max_concurrency = 1;
    config.rate_limit.enabled = false;

    // The handler signals when it holds the only permit, so the test cannot
    // race ahead of the limiter.
    let (entered_tx, entered_rx) = tokio::sync::watch::channel(false);
    let entered_tx = Arc::new(entered_tx);
    let handler_tx = entered_tx.clone();
    let (url, task) = spawn_real(move || {
        let tx = handler_tx.clone();
        let app = App::new().route(
            "/hold",
            web::get().to(move || {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(true);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    "done"
                }
            }),
        );
        middleware::stack(
            app,
            config.clone(),
            None,
            None,
            None,
            None,
            None,
            sanvi_http::ssr_cache::SsrCacheLayer::disabled(
                sanvi_http::config::SsrCacheConfig::default(),
            ),
            sanvi_http::degraded::DegradedHeaderLayer::disabled(),
            None,
        )
    })
    .await;

    let first = tokio::spawn(raw_get(format!("{url}/hold")));
    let mut entered_rx = entered_rx;
    while !*entered_rx.borrow() {
        if entered_rx.changed().await.is_err() {
            break;
        }
    }

    let (status2, _, _) = raw_get(format!("{url}/hold")).await;
    assert_eq!(status2, 503);

    let (status1, _, _) = first.await.unwrap();
    assert_eq!(status1, 200);
    task.abort();
}

#[actix_web::test]
async fn concurrency_rejection_carries_request_id_and_retry_after() {
    let mut config = test_config();
    config.max_concurrency = 1;
    config.rate_limit.enabled = false;

    let (entered_tx, entered_rx) = tokio::sync::watch::channel(false);
    let entered_tx = Arc::new(entered_tx);
    let handler_tx = entered_tx.clone();
    let (url, task) = spawn_real(move || {
        let tx = handler_tx.clone();
        let app = App::new().route(
            "/hold",
            web::get().to(move || {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(true);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    "done"
                }
            }),
        );
        middleware::stack(
            app,
            config.clone(),
            None,
            None,
            None,
            None,
            None,
            sanvi_http::ssr_cache::SsrCacheLayer::disabled(
                sanvi_http::config::SsrCacheConfig::default(),
            ),
            sanvi_http::degraded::DegradedHeaderLayer::disabled(),
            None,
        )
    })
    .await;

    let first = tokio::spawn(raw_get(format!("{url}/hold")));
    let mut entered_rx = entered_rx;
    while !*entered_rx.borrow() {
        if entered_rx.changed().await.is_err() {
            break;
        }
    }

    let (status, headers, _) = raw_get(format!("{url}/hold")).await;
    assert_eq!(status, 503);
    assert!(
        headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-request-id"))
    );
    assert!(
        headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
    );

    first.await.unwrap();
    task.abort();
}

/// The middleware chain must keep bodies consumable on every path.
#[actix_web::test]
async fn rejection_bodies_are_readable_json() {
    let app = test::init_service(build_app(test_config())).await;
    let req = test::TestRequest::get().uri("/ok").to_request();
    let res = test::call_service(&app, req).await;
    let bytes = actix_web::body::to_bytes(res.into_body()).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], 503);
}
