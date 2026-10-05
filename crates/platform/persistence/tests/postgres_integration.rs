//! Integration tests: real Postgres. Run with `just test` (docker must be
//! available).

use std::time::Duration;

use sanvi_kernel::SnowflakeId;
use sanvi_persistence::{
    DatabaseConfig, Migrator, TxManager, build_pool, check_database, is_statement_timeout,
};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

fn migrations_dir() -> String {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("../../../migrations")
        .canonicalize()
        .expect("migrations directory must exist")
        .to_string_lossy()
        .to_string()
}

fn config_for(port: u16, user: &str, password: &str) -> DatabaseConfig {
    DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "postgres".to_string(),
        user: user.to_string(),
        password: password.to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    }
}

async fn assert_column_type(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
    column: &str,
    expected: &str,
) {
    let actual: String = sqlx::query_scalar(
        "SELECT data_type
         FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .unwrap();

    assert_eq!(
        actual, expected,
        "{schema}.{table}.{column} must use {expected}"
    );
}

#[tokio::test]
async fn migrations_run_and_revert_cleanly() {
    // A database with the two roles in place but nothing applied: this test
    // is the one that has to run the migrations itself, up and back down.
    let db = sanvi_testsupport::blank_db().await;
    let migrator_pool = db.migrator_pool().await;
    let pool = db.app_pool();

    let migrator = Migrator::new(sanvi_testsupport::migrations_dir());
    migrator.run(&migrator_pool).await.expect("migrations up");

    // Outbox table exists and the app role can write to it.
    let exists: (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'platform' AND table_name = 'outbox')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(exists.0, "platform.outbox must exist after migration");

    assert_column_type(&pool, "tenancy", "tenants", "id", "bigint").await;
    assert_column_type(&pool, "identity", "users", "id", "bigint").await;
    assert_column_type(&pool, "identity", "users", "kratos_identity_id", "uuid").await;
    assert_column_type(&pool, "platform", "outbox", "id", "bigint").await;
    assert_column_type(&pool, "platform", "processed_events", "event_id", "bigint").await;
    assert_column_type(&pool, "platform", "audit_log", "id", "bigint").await;

    sqlx::query(
        "INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload)
         VALUES ($1, 'test.aggregate', $2, 'test.event', '{}')",
    )
    .bind(101_i64)
    .bind(202_i64)
    .execute(&pool)
    .await
    .expect("app role must be able to write to the outbox");

    // Readiness probe works against the real pool.
    check_database(&pool, Duration::from_secs(5))
        .await
        .expect("health probe");

    // Down migration returns the schema to a clean state.
    migrator
        .undo(&migrator_pool, 0)
        .await
        .expect("migrations down");
    let exists: (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'platform')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!exists.0, "platform schema must be gone after undo");

    pool.close().await;
}

#[tokio::test]
async fn platform_scope_writes_a_snowflake_audit_id() {
    let db = sanvi_testsupport::test_db().await;
    let manager = TxManager::new(db.app_pool(), sanvi_testsupport::test_ids());

    manager
        .begin_unscoped("test", "tester")
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    let admin = db.admin_pool().await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM platform.audit_log LIMIT 1")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert!(SnowflakeId::try_from(id).is_ok());
}

#[tokio::test]
async fn health_probe_degrades_when_database_is_gone() {
    let node = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres");
    let port = node.get_host_port_ipv4(5432).await.expect("postgres port");
    let pool = build_pool(&config_for(port, "postgres", "postgres"))
        .await
        .unwrap();

    check_database(&pool, Duration::from_secs(5))
        .await
        .expect("healthy while up");

    node.stop().await.expect("stopping container");
    let result = check_database(&pool, Duration::from_secs(2)).await;
    assert!(
        result.is_err(),
        "probe must fail once the database is unreachable"
    );
}

/// TASK-021 (FR-1110): the pool-level statement cap cancels a runaway query
/// and the failure carries the classification the HTTP layer turns into the
/// `request-timeout` problem — a request outside its budget fails loudly
/// instead of holding its connection.
#[tokio::test]
async fn statement_timeout_cancels_runaway_queries() {
    let node = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres");
    let port = node.get_host_port_ipv4(5432).await.expect("postgres port");
    let config = DatabaseConfig {
        statement_timeout_ms: 1_000,
        ..config_for(port, "postgres", "postgres")
    };
    let pool = build_pool(&config).await.unwrap();

    let started = std::time::Instant::now();
    let result = sqlx::query("SELECT pg_sleep(30)").execute(&pool).await;
    let elapsed = started.elapsed();

    let err = result.expect_err("pg_sleep(30) must be cancelled by the 1 s cap");
    assert!(
        is_statement_timeout(&err),
        "classified as a statement timeout: {err}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "cancelled near the cap, not after the full sleep: {elapsed:?}"
    );

    // The rendered message is what adapters flatten into Storage(String);
    // the HTTP classification matches this exact phrase, so pin the join.
    let rendered = err.to_string();
    assert!(
        rendered.contains("canceling statement due to statement timeout"),
        "rendered message carries the Postgres wording: {rendered}"
    );

    // The connection survives: the cap cancels statements, not sessions, so
    // the pool serves the next query without a recycle.
    let alive: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&pool)
        .await
        .expect("pool still serves after a cancelled statement");
    assert_eq!(alive, 1);
}
