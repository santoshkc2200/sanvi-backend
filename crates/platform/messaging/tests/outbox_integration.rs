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

    // …and the event reached the stream with the same id as the outbox row.
    let entries = streams.range("sanvi.events", "-", "+").await.unwrap();
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
