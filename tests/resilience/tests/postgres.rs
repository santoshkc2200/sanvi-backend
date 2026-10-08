//! Dependency-matrix injection tests: Postgres (TASK-023, FR-1112).
//!
//! Slow injects a real statement timeout against the test database;
//! erroring terminates the session's backend (`57P01`, genuinely
//! transient-classified); down points a pool at a port nothing listens
//! on. Each test asserts the matrix row's documented behaviour.

mod common;

use sanvi_http::ApiError;
use sanvi_messaging::is_transient_error;

/// Postgres · slow — the statement timeout cancels the query and the
/// adapter-level mapping renders it as the `request-timeout` problem
/// (504).
#[tokio::test]
async fn postgres_slow_maps_statement_timeout_to_request_timeout() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let mut conn = pool.acquire().await.expect("pool connection");
    sqlx::query("SET statement_timeout = 200")
        .execute(&mut *conn)
        .await
        .expect("set session statement timeout");

    let err = sqlx::query("SELECT pg_sleep(1)")
        .execute(&mut *conn)
        .await
        .expect_err("pg_sleep(1) exceeds the 200ms statement timeout");
    let rendered = err.to_string();
    assert!(
        rendered.contains("canceling statement due to statement timeout"),
        "unexpected error text: {rendered}"
    );

    let mapped = ApiError::from_storage_detail(&rendered);
    assert_eq!(mapped.status().as_u16(), 504);
    assert_eq!(mapped.type_slug(), "request-timeout");
    assert_eq!(mapped.title(), "Gateway Timeout");
}

/// Postgres · erroring — a backend terminated under us (`57P01`) classifies
/// as transient (retryable by the messaging layer); a plain constraint
/// violation does not.
#[tokio::test]
async fn postgres_transient_sqlstates_classify_as_retryable() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let mut victim = pool.acquire().await.expect("victim connection");
    let victim_pid: (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *victim)
        .await
        .unwrap();

    // A second connection kills the first one's backend: the victim's next
    // statement fails with 57P01 (terminating connection due to
    // administrator command), a transient-class SQLSTATE.
    let killer_pool = pool.clone();
    let killer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut conn = killer_pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(victim_pid.0)
            .execute(&mut *conn)
            .await
            .expect("terminate victim backend");
    });
    let err = sqlx::query("SELECT pg_sleep(2)")
        .execute(&mut *victim)
        .await
        .expect_err("the terminated backend's query must fail");
    killer.await.unwrap();
    let rendered = err.to_string();
    assert!(
        rendered.contains("57P01") || rendered.contains("terminating connection"),
        "unexpected error text: {rendered}"
    );
    let transient: anyhow::Error = err.into();
    assert!(
        is_transient_error(&transient),
        "57P01 must classify as transient: {rendered}"
    );

    // A data error is permanent: no retry can fix it. (No DDL here on
    // purpose: the app role cannot create tables, and a permanent error
    // needs no setup — division by zero, `22012`, always fails the same
    // way.)
    let data_error: anyhow::Error = sqlx::query("SELECT 1/0")
        .execute(&pool)
        .await
        .expect_err("division by zero must fail")
        .into();
    assert!(
        !is_transient_error(&data_error),
        "22012 must not classify as transient"
    );
}

/// Postgres · down — a pool pointed at a dead port fails its acquire, and
/// the failure surfaces opaque (internal-error, 500): infrastructure
/// detail is for the logs, not the client. The pool's acquire timeout
/// bounds the wait rather than hanging the request.
#[tokio::test]
async fn postgres_down_fails_opaque_while_pool_bounds_the_wait() {
    let (_base, port) = common::closed_endpoint();
    let dsn = format!("postgres://sanvi:sanvi@127.0.0.1:{port}/sanvi_down");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(500))
        .connect_lazy(&dsn)
        .expect("lazy pool builds without contacting the server");

    let started = std::time::Instant::now();
    let err = sqlx::query("SELECT 1")
        .fetch_one(&pool)
        .await
        .expect_err("a dead Postgres must fail the query");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the acquire timeout must bound the wait, took {elapsed:?}"
    );

    // The documented user-visible consequence: opaque 500, never a leak of
    // the DSN or the infrastructure error text.
    let mapped = ApiError::from_storage_detail(&err.to_string());
    assert_eq!(mapped.status().as_u16(), 500);
    assert_eq!(mapped.type_slug(), "internal-error");
    let problem = mapped.problem();
    assert!(
        !problem.detail.contains(&dsn),
        "the problem body must not carry the DSN: {:?}",
        problem.detail
    );
}
