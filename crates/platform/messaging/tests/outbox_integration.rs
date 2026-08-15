//! Integration tests: real Postgres and Redis in containers. Run with
//! `cargo nextest run` (docker must be available).

use sanvi_messaging::{OutboxDispatcher, RedisConfig, StreamName, StreamsClient, connect_redis};
use sanvi_persistence::{DatabaseConfig, Migrator, build_pool};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::{postgres::Postgres, redis::Redis};

fn migrations_dir() -> String {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("../../../migrations")
        .canonicalize()
        .expect("migrations directory must exist")
        .to_string_lossy()
        .to_string()
}

async fn prepare_postgres() -> (
    sanvi_persistence::PgPool,
    testcontainers::ContainerAsync<Postgres>,
) {
    let node = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres");
    let port = node.get_host_port_ipv4(5432).await.expect("postgres port");

    let admin = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
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
    sqlx::query("GRANT CONNECT ON DATABASE sanvi_test TO sanvi_app")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;

    let migrator_pool = build_pool(&DatabaseConfig {
        name: "sanvi_test".to_string(),
        user: "sanvi_migrator".to_string(),
        password: "mig".to_string(),
        ..DatabaseConfig {
            host: "127.0.0.1".to_string(),
            port,
            name: "postgres".to_string(),
            user: String::new(),
            password: String::new(),
            migrations_dir: migrations_dir(),
            ..DatabaseConfig::default()
        }
    })
    .await
    .unwrap();
    Migrator::new(migrations_dir())
        .run(&migrator_pool)
        .await
        .expect("migrations up");
    migrator_pool.close().await;

    let app_pool = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_app".to_string(),
        password: "app".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();

    (app_pool, node)
}

#[tokio::test]
async fn dispatcher_relays_outbox_rows_to_the_stream() {
    // The container handle must stay alive for the whole test: dropping it
    // stops Postgres underneath the pool.
    let (pool, _pg_node) = prepare_postgres().await;

    let redis_node = Redis::default()
        .start()
        .await
        .expect("failed to start redis");
    let redis_port = redis_node
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");

    let streams = StreamsClient::new(
        connect_redis(&RedisConfig {
            url: format!("redis://127.0.0.1:{redis_port}"),
            ..RedisConfig::default()
        })
        .await
        .unwrap(),
    );

    // Insert an unpublished domain event.
    let event_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload)
         VALUES ($1, 'test.aggregate', $2, 'test.something_happened', $3::jsonb)",
    )
    .bind(event_id)
    .bind(uuid::Uuid::now_v7())
    .bind(r#"{"k":"v"}"#)
    .execute(&pool)
    .await
    .unwrap();

    let dispatcher =
        OutboxDispatcher::new(pool.clone(), streams.clone(), StreamName::default(), 50, 5);

    let published = dispatcher.run_once().await.expect("dispatch");
    assert_eq!(published, 1);

    // The row is marked published…
    let published_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT published_at FROM platform.outbox WHERE id = $1")
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(published_at.is_some());

    // …and the event reached the per-context stream (`{base}.{aggregate
    // context}`) with the same id as the outbox row.
    let entries = streams.range("sanvi.events.test", "-", "+").await.unwrap();
    assert_eq!(entries.len(), 1);
    let (_, fields) = &entries[0];
    let event_json = fields
        .iter()
        .find(|(k, _)| k == "event")
        .map(|(_, v)| v.clone())
        .unwrap();
    let envelope: sanvi_kernel::EventEnvelope = serde_json::from_str(&event_json).unwrap();
    assert_eq!(envelope.id, event_id);
    assert_eq!(envelope.event_type, "test.something_happened");

    // Second pass finds nothing new.
    assert_eq!(dispatcher.run_once().await.unwrap(), 0);
}

/// Regression test for the claim transaction: two dispatcher replicas racing
/// on the same unpublished rows must never both claim (and republish) the
/// same row. Before the `SELECT ... FOR UPDATE SKIP LOCKED` claim ran inside
/// a transaction that stays open until the batch is marked published, the
/// row lock was released the instant the SELECT returned, so a concurrent
/// `run_once()` could see the same "unlocked" rows and double-publish them.
#[tokio::test]
async fn concurrent_dispatchers_never_double_claim_the_same_row() {
    let (pool, _pg_node) = prepare_postgres().await;

    let redis_node = Redis::default()
        .start()
        .await
        .expect("failed to start redis");
    let redis_port = redis_node
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");

    let streams = StreamsClient::new(
        connect_redis(&RedisConfig {
            url: format!("redis://127.0.0.1:{redis_port}"),
            ..RedisConfig::default()
        })
        .await
        .unwrap(),
    );

    const ROWS: usize = 20;
    let mut event_ids = Vec::with_capacity(ROWS);
    for _ in 0..ROWS {
        let event_id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload)
             VALUES ($1, 'test.aggregate', $2, 'test.something_happened', $3::jsonb)",
        )
        .bind(event_id)
        .bind(uuid::Uuid::now_v7())
        .bind(r#"{"k":"v"}"#)
        .execute(&pool)
        .await
        .unwrap();
        event_ids.push(event_id);
    }

    let stream = StreamName("sanvi.concurrent-test".to_string());
    // A large batch size relative to ROWS: unprotected, either dispatcher
    // could greedily claim the whole table on its own.
    let dispatcher_a = OutboxDispatcher::new(pool.clone(), streams.clone(), stream.clone(), 50, 5);
    let dispatcher_b = OutboxDispatcher::new(pool.clone(), streams.clone(), stream.clone(), 50, 5);

    let (published_a, published_b) = tokio::join!(dispatcher_a.run_once(), dispatcher_b.run_once());
    let total_published = published_a.expect("dispatch a") + published_b.expect("dispatch b");
    assert_eq!(
        total_published, ROWS,
        "every row must be published exactly once across both dispatchers"
    );

    let entries = streams
        .range("sanvi.concurrent-test.test", "-", "+")
        .await
        .unwrap();
    assert_eq!(
        entries.len(),
        ROWS,
        "the stream must contain exactly one entry per row, no duplicates"
    );

    let published_ids: std::collections::HashSet<uuid::Uuid> = entries
        .iter()
        .map(|(_, fields)| {
            let event_json = fields
                .iter()
                .find(|(k, _)| k == "event")
                .map(|(_, v)| v.clone())
                .unwrap();
            let envelope: sanvi_kernel::EventEnvelope = serde_json::from_str(&event_json).unwrap();
            envelope.id
        })
        .collect();
    assert_eq!(
        published_ids.len(),
        ROWS,
        "every published id must be unique"
    );
    assert_eq!(published_ids, event_ids.into_iter().collect());
}

#[tokio::test]
async fn consumer_group_creation_is_idempotent() {
    let redis_node = Redis::default()
        .start()
        .await
        .expect("failed to start redis");
    let redis_port = redis_node
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let streams = StreamsClient::new(
        connect_redis(&RedisConfig {
            url: format!("redis://127.0.0.1:{redis_port}"),
            ..RedisConfig::default()
        })
        .await
        .unwrap(),
    );

    streams
        .ensure_consumer_group("sanvi.test", "consumer-a")
        .await
        .unwrap();
    streams
        .ensure_consumer_group("sanvi.test", "consumer-a")
        .await
        .unwrap();
    streams
        .ensure_consumer_group("sanvi.test", "consumer-b")
        .await
        .unwrap();
}
