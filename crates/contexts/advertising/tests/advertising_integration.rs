//! Advertising schema integration tests.
//!
//! These tests use an isolated Postgres container so the phase-10 migration
//! rollback and partition RLS checks never operate on a developer database.

use chrono::{DateTime, Datelike, Months, Utc};
use sanvi_persistence::{DatabaseConfig, Migrator, TxManager, build_pool};
use sqlx::{Postgres as SqlxPostgres, QueryBuilder};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

const ADVERTISING_MIGRATION: i64 = 20_260_826_000_000;

fn migrations_dir() -> std::path::PathBuf {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join("../../../migrations"))
        .unwrap_or_else(|_| std::path::PathBuf::from("migrations"))
}

async fn start_postgres() -> (u16, testcontainers::ContainerAsync<Postgres>) {
    let node = Postgres::default().start().await.unwrap();
    let port = node.get_host_port_ipv4(5432).await.unwrap();
    let admin = sqlx::PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
    ))
    .await
    .unwrap();
    sqlx::query("CREATE ROLE sanvi_migrator LOGIN PASSWORD 'mig'")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE ROLE sanvi_app LOGIN PASSWORD 'app' NOBYPASSRLS")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE sanvi_test OWNER sanvi_migrator")
        .execute(&admin)
        .await
        .unwrap();
    (port, node)
}

fn db_config(port: u16, user: &str, password: &str) -> DatabaseConfig {
    DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: user.to_string(),
        password: password.to_string(),
        ..DatabaseConfig::default()
    }
}

async fn migrate(port: u16) -> sqlx::PgPool {
    let config = db_config(port, "sanvi_migrator", "mig");
    let pool = sqlx::PgPool::connect(&format!(
        "postgres://{}:{}@127.0.0.1:{}/{}",
        config.user, config.password, port, config.name
    ))
    .await
    .unwrap();
    Migrator::new(migrations_dir()).run(&pool).await.unwrap();
    pool
}

async fn app_pool(port: u16) -> sanvi_persistence::PgPool {
    build_pool(&db_config(port, "sanvi_app", "app"))
        .await
        .unwrap()
}

async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> Uuid {
    let tenant_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, $2, $3, 'active', 'jp', 'en')",
    )
    .bind(tenant_id)
    .bind(slug)
    .bind(format!("{slug} corp"))
    .execute(pool)
    .await
    .unwrap();
    tenant_id
}

async fn insert_conversion_event(
    pool: &sanvi_persistence::PgPool,
    tenant_id: Uuid,
    connection_id: Uuid,
) -> chrono::DateTime<Utc> {
    let manager = TxManager::new(pool.clone());
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    let tx = scope.tx_mut().as_inner();
    let event_id = format!("event-{}", Uuid::now_v7());
    let occurred_at = Utc::now();

    sqlx::query(
        "INSERT INTO advertising.connections
         (id, tenant_id, platform, external_account_id, currency, timezone, status, token_ref)
         VALUES ($1, $2, 'google_ads', $3, 'JPY', 'Asia/Tokyo', 'active', 'secret-ref')",
    )
    .bind(connection_id)
    .bind(tenant_id)
    .bind(format!("acct-{}", connection_id.simple()))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.campaigns
         (id, tenant_id, connection_id, platform, name, objective, status, budget)
         VALUES ($1, $2, $3, 'google_ads', 'Campaign', 'sales', 'draft', '{}'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(connection_id)
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_event_dedup (tenant_id, event_id, occurred_at)
         VALUES ($1, $2, $3)",
    )
    .bind(tenant_id)
    .bind(&event_id)
    .bind(occurred_at)
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_events
         (id, tenant_id, event_id, name, occurred_at, consent)
         VALUES ($1, $2, $3, 'purchase', $4, '{}'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(event_id)
    .bind(occurred_at)
    .execute(&mut **tx)
    .await
    .unwrap();
    scope.commit().await.unwrap();
    occurred_at
}

fn monthly_partition_names(date: DateTime<Utc>) -> [String; 2] {
    let suffix = format!("{:04}{:02}", date.year(), date.month());
    [
        format!("advertising.conversion_events_{suffix}"),
        format!("advertising.metrics_daily_{suffix}"),
    ]
}

async fn partition_exists(pool: &sqlx::PgPool, partition: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(partition)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn partition_row_count(tx: &mut sqlx::Transaction<'_, SqlxPostgres>, partition: &str) -> i64 {
    // `partition` is generated only by this test from a UTC month; it never
    // contains user input. QueryBuilder keeps that audited identifier path
    // distinct from the literal SQL used by application code.
    let mut query = QueryBuilder::<SqlxPostgres>::new("SELECT COUNT(*) FROM ");
    query.push(partition);
    query
        .build_query_scalar()
        .fetch_one(&mut **tx)
        .await
        .unwrap()
}

#[tokio::test]
async fn advertising_migration_round_trips_on_a_seeded_database() {
    let (port, _node) = start_postgres().await;
    let migrator_pool = migrate(port).await;
    let pool = app_pool(port).await;
    seed_tenant(&pool, "advertising-round-trip").await;
    let now = Utc::now();
    let next_month = now
        .checked_add_months(Months::new(1))
        .expect("a date one month from now must be representable");
    let expected_partitions = [
        monthly_partition_names(now),
        monthly_partition_names(next_month),
    ]
    .concat();

    for partition in &expected_partitions {
        assert!(
            partition_exists(&migrator_pool, partition).await,
            "{partition} must exist after migration"
        );
    }

    let migrator = Migrator::new(migrations_dir());
    migrator
        .undo(&migrator_pool, ADVERTISING_MIGRATION - 1)
        .await
        .unwrap();

    let schema_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'advertising')",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(!schema_exists, "advertising schema must be gone after undo");
    for partition in &expected_partitions {
        assert!(
            !partition_exists(&migrator_pool, partition).await,
            "{partition} must be gone after undo"
        );
    }

    let tenant_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenancy.tenants")
        .fetch_one(&migrator_pool)
        .await
        .unwrap();
    assert_eq!(tenant_count, 1, "earlier migrations must survive the undo");

    migrator.run(&migrator_pool).await.unwrap();
    let schema_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'advertising')",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(
        schema_exists,
        "advertising schema must return after reapply"
    );
    for partition in &expected_partitions {
        assert!(
            partition_exists(&migrator_pool, partition).await,
            "{partition} must return after reapply"
        );
    }
}

#[tokio::test]
async fn tenant_scope_hides_other_tenants_connections_and_partition_rows() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_a = seed_tenant(&pool, "advertising-alpha").await;
    let tenant_b = seed_tenant(&pool, "advertising-beta").await;

    let occurred_at = insert_conversion_event(&pool, tenant_a, Uuid::now_v7()).await;

    let partition = format!(
        "advertising.conversion_events_{:04}{:02}",
        occurred_at.year(),
        occurred_at.month()
    );
    let manager = TxManager::new(pool);

    {
        let mut tenant_a_scope = manager.begin_for(tenant_a).await.unwrap();
        let tx = tenant_a_scope.tx_mut().as_inner();
        let connection_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM advertising.connections")
                .fetch_one(&mut **tx)
                .await
                .unwrap();
        assert_eq!(connection_count, 1, "tenant A must see its own connection");
        let campaign_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM advertising.campaigns")
            .fetch_one(&mut **tx)
            .await
            .unwrap();
        assert_eq!(campaign_count, 1, "tenant A must see its own campaign");
        let event_count = partition_row_count(tx, &partition).await;
        assert_eq!(event_count, 1, "tenant A must see its own partition row");
        tenant_a_scope.rollback().await.unwrap();
    }

    let mut tenant_b_scope = manager.begin_for(tenant_b).await.unwrap();
    let tx = tenant_b_scope.tx_mut().as_inner();
    let connection_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM advertising.connections")
        .fetch_one(&mut **tx)
        .await
        .unwrap();
    assert_eq!(
        connection_count, 0,
        "tenant B must not see tenant A's connection"
    );
    let campaign_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM advertising.campaigns")
        .fetch_one(&mut **tx)
        .await
        .unwrap();
    assert_eq!(
        campaign_count, 0,
        "tenant B must not see tenant A's campaign"
    );
    let event_count = partition_row_count(tx, &partition).await;
    assert_eq!(
        event_count, 0,
        "tenant B must not see tenant A's partition row"
    );
    tenant_b_scope.rollback().await.unwrap();
}
