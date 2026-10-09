//! Probe contract tests (TASK-025, FR-1118): the two probes a restore and
//! a deploy both depend on.
//!
//! - `GET /api/v1/system/health` is liveness only. The database going away
//!   must not take it down — that is the classic outage amplifier where a
//!   slow database marks every instance unhealthy and the orchestrator
//!   kills them all. The test drops the database under a live probe and
//!   asserts the endpoint keeps answering (and readiness degrades).
//! - `GET /api/v1/system/ready` reports states. A readiness endpoint is a
//!   reconnaissance endpoint if it is honest to the wrong audience, so the
//!   snapshot asserts the response carries no hostname, version or error
//!   string — even when the underlying probe error contains all three.
//! - The readiness fan-out cannot hang the endpoint: the handler's own
//!   response timeout answers degraded when a probe wedges past its budget.

use std::sync::Arc;
use std::time::Duration;

use actix_web::{
    App,
    body::BoxBody,
    dev::{ServiceFactory, ServiceRequest, ServiceResponse},
    http::StatusCode,
    test, web,
};
use sanvi_system::{
    adapters::http::{SystemState, api_routes, health_routes},
    adapters::probes::PostgresProbe,
    application::{CheckLiveness, CheckReadiness},
    domain::{HealthProbe, SystemError},
};

/// The probe surface, scoped exactly as `build_app` mounts it.
fn probe_app(
    state: SystemState,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<BoxBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(web::Data::new(state))
        .configure(health_routes)
        .service(web::scope("/api/v1/system").configure(api_routes))
}

/// System state over explicit probes — the drill's shape, where the
/// database is the only dependency.
fn state_with_probes(probes: Vec<Arc<dyn HealthProbe>>, budget: Duration) -> SystemState {
    let build_info = sanvi_system::domain::BuildInfo::new(
        "0.0.0-test".to_string(),
        "test".to_string(),
        "test".to_string(),
        "test".to_string(),
        "probe-tests".to_string(),
    );
    SystemState {
        build_details: Arc::new(sanvi_system::application::GetBuildDetails::new(
            &build_info,
            "test".to_string(),
        )),
        build_info: Arc::new(sanvi_system::application::GetBuildInfo::new(build_info)),
        readiness: Arc::new(CheckReadiness::new(probes, budget)),
        liveness: Arc::new(CheckLiveness::new()),
    }
}

/// Drop the test's own database `FORCE`, terminating the pool's sessions
/// server-side — for the process whose pool points at it, the database is
/// now down in the strongest local sense available.
async fn drop_database(port: u16, name: &str) {
    use sqlx::{AssertSqlSafe, Connection, PgConnection, postgres::PgConnectOptions};
    let mut admin = PgConnection::connect_with(
        &PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .database("postgres")
            .username("postgres")
            .password("postgres")
            .application_name("system-tests"),
    )
    .await
    .expect("admin connection");
    let statement = format!("DROP DATABASE {name} WITH (FORCE)");
    sqlx::raw_sql(AssertSqlSafe(statement))
        .execute(&mut admin)
        .await
        .expect("drop database");
    admin.close().await.expect("close admin connection");
}

/// The step the task exists for: the database is stopped, `/health` still
/// returns 200 — and `/ready` is the endpoint that tells the truth about
/// the dependency.
#[actix_web::test]
async fn health_returns_while_the_database_is_down() {
    let db = sanvi_testsupport::test_db().await;
    let state = state_with_probes(
        vec![Arc::new(PostgresProbe::new(db.app_pool()))],
        Duration::from_secs(2),
    );
    let app = test::init_service(probe_app(state)).await;

    // Baseline: alive, and ready — the database answers.
    let req = test::TestRequest::get()
        .uri("/api/v1/system/health")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK, "health must start 200");

    let req = test::TestRequest::get()
        .uri("/api/v1/system/ready")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK, "ready must start 200");

    // Stop the database.
    let (port, name) = (db.port(), db.name().to_string());
    drop_database(port, &name).await;

    // Liveness must not have noticed: it never touches the database.
    let req = test::TestRequest::get()
        .uri("/api/v1/system/health")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "/health must return 200 with the database stopped"
    );

    let req = test::TestRequest::get()
        .uri("/api/v1/system/ready")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["checks"][0]["name"], "database");
    assert_eq!(body["checks"][0]["state"], "degraded");
    assert!(
        body["checks"][0].get("detail").is_none(),
        "/ready must not carry a detail member"
    );
}

/// A probe whose failure detail is exactly what must never reach `/ready`:
/// a hostname, a version, an error string.
struct LeakyProbe(&'static str);

#[async_trait::async_trait]
impl HealthProbe for LeakyProbe {
    fn name(&self) -> &'static str {
        self.0
    }
    async fn check(&self, _budget: Duration) -> Result<(), SystemError> {
        Err(SystemError::DependencyUnavailable(
            self.0.to_string(),
            "db-host.internal:5432 (postgres server version 16.3): connection refused".to_string(),
        ))
    }
}

struct OkProbe(&'static str);

#[async_trait::async_trait]
impl HealthProbe for OkProbe {
    fn name(&self) -> &'static str {
        self.0
    }
    async fn check(&self, _budget: Duration) -> Result<(), SystemError> {
        Ok(())
    }
}

/// The leak snapshot: the degraded `/ready` body is exactly the states —
/// no `detail` member anywhere, none of the probe error's host, version
/// or error text anywhere in the response.
#[actix_web::test]
async fn ready_reports_states_never_hostnames_versions_or_errors() {
    let state = state_with_probes(
        vec![
            Arc::new(LeakyProbe("database")),
            Arc::new(LeakyProbe("redis")),
        ],
        Duration::from_secs(1),
    );
    let app = test::init_service(probe_app(state)).await;

    let req = test::TestRequest::get()
        .uri("/api/v1/system/ready")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

    let body = test::read_body(res).await;
    let raw = String::from_utf8(body.to_vec()).expect("body is utf-8");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("body is json");
    assert_eq!(
        json,
        serde_json::json!({
            "status": "degraded",
            "checks": [
                {"name": "database", "state": "degraded"},
                {"name": "redis", "state": "degraded"}
            ]
        }),
        "response shape changed — re-audit the leak assertions below before updating"
    );
    for leak in [
        "db-host.internal",
        "5432",
        "16.3",
        "connection refused",
        "detail",
        "0.0.0-test",
    ] {
        assert!(
            !raw.contains(leak),
            "/ready leaked {leak:?} in the response body"
        );
    }
}

/// The healthy path is states too, and carries no build/version stamp.
#[actix_web::test]
async fn ready_healthy_path_is_states_only() {
    let state = state_with_probes(
        vec![Arc::new(OkProbe("database")), Arc::new(OkProbe("redis"))],
        Duration::from_secs(1),
    );
    let app = test::init_service(probe_app(state)).await;

    let req = test::TestRequest::get()
        .uri("/api/v1/system/ready")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let json: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(
        json,
        serde_json::json!({
            "status": "ok",
            "checks": [
                {"name": "database", "state": "ok"},
                {"name": "redis", "state": "ok"}
            ]
        })
    );
}

/// A probe that ignores its budget cannot hang the endpoint: the
/// handler's own response timeout answers degraded within
/// `budget + grace`.
#[actix_web::test]
async fn wedged_probe_cannot_hang_ready() {
    struct WedgedProbe;
    #[async_trait::async_trait]
    impl HealthProbe for WedgedProbe {
        fn name(&self) -> &'static str {
            "database"
        }
        async fn check(&self, _budget: Duration) -> Result<(), SystemError> {
            // Deliberately ignores the budget the port hands it.
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
    }

    let state = state_with_probes(vec![Arc::new(WedgedProbe)], Duration::from_millis(100));
    let app = test::init_service(probe_app(state)).await;

    let started = std::time::Instant::now();
    let req = test::TestRequest::get()
        .uri("/api/v1/system/ready")
        .to_request();
    let res = test::call_service(&app, req).await;
    let elapsed = started.elapsed();

    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        elapsed < Duration::from_secs(5),
        "/ready took {elapsed:?} against a 100ms budget — the response timeout is not holding"
    );
    let json: serde_json::Value = test::read_body_json(res).await;
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["checks"][0]["name"], "database");
    assert_eq!(json["checks"][0]["state"], "degraded");
}
