//! Integration tests: stream consumers against a real Postgres and Redis.
//! Run with `just test` (docker must be available).

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use sanvi_messaging::{ConsumerMetrics, RedisConfig, StreamConsumer, StreamsClient, connect_redis};
use sanvi_testsupport::TestDb;

async fn prepare_postgres() -> (sanvi_persistence::PgPool, TestDb) {
    let db = sanvi_testsupport::test_db().await;
    (db.app_pool(), db)
}

/// A `StreamsClient` on the shared Redis. Key spaces are kept apart by
/// stream name (`sanvi_testsupport::test_stream`), not by server or database.
async fn prepare_redis() -> StreamsClient {
    StreamsClient::new(
        connect_redis(&RedisConfig {
            url: sanvi_testsupport::redis_url().await,
            ..RedisConfig::default()
        })
        .await
        .unwrap(),
    )
}

#[tokio::test]
async fn consumer_processes_each_event_exactly_once_across_redelivery() {
    let (pool, _db) = prepare_postgres().await;
    let streams = prepare_redis().await;

    let handled = Arc::new(AtomicUsize::new(0));
    let stream = sanvi_testsupport::test_stream("tenancy");

    let envelope = sanvi_kernel::EventEnvelope::new(
        sanvi_kernel::EventId::try_from(101_i64).unwrap(),
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(201_i64).unwrap(),
        "tenancy.tenant_activated",
        serde_json::json!({ "slug": "acme" }),
    );
    let payload = serde_json::to_string(&envelope).unwrap();

    // Deliver the same event twice (simulates at-least-once redelivery).
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    // Process until both entries are consumed (the consumer acks them).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer_name = format!("tenancy-test-{}", uuid::Uuid::now_v7());
    let run = {
        let handled = handled.clone();
        let pool = pool.clone();
        let streams = streams.clone();
        let stream = stream.clone();
        tokio::spawn(async move {
            let consumer = StreamConsumer::new(pool.clone(), streams.clone());
            consumer
                .run_until_shutdown(
                    &stream,
                    "tenancy",
                    &consumer_name,
                    Arc::new(move |envelope| {
                        let handled = handled.clone();
                        Box::pin(async move {
                            handled.fetch_add(1, Ordering::SeqCst);
                            let _ = envelope;
                            Ok(())
                        })
                    }),
                    shutdown_rx,
                )
                .await;
        })
    };

    // Give it a moment, then stop and assert.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let _ = shutdown_tx.send(true);
    run.await.unwrap();

    // At-least-once delivery collapsed to exactly-once processing.
    assert_eq!(handled.load(Ordering::SeqCst), 1);

    // Dedup rows recorded; pending empty after acks.
    let processed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.processed_events WHERE consumer = 'tenancy'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(processed, 1);
}

#[tokio::test]
async fn failing_handler_dead_letters_the_event_and_unblocks_the_group() {
    let (pool, _db) = prepare_postgres().await;
    let streams = prepare_redis().await;

    let consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let stream = sanvi_testsupport::test_stream("broken");

    let envelope = sanvi_kernel::EventEnvelope::new(
        sanvi_kernel::EventId::try_from(102_i64).unwrap(),
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(202_i64).unwrap(),
        "tenancy.tenant_archived",
        serde_json::json!({ "slug": "doomed" }),
    );
    let payload = serde_json::to_string(&envelope).unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer_name = format!("broken-test-{}", uuid::Uuid::now_v7());
    let run = tokio::spawn({
        let stream = stream.clone();
        async move {
            consumer
                .run_until_shutdown(
                    &stream,
                    "broken",
                    &consumer_name,
                    Arc::new(move |_envelope| {
                        Box::pin(async move {
                            anyhow::bail!("handler exploded");
                        })
                    }),
                    shutdown_rx,
                )
                .await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let _ = shutdown_tx.send(true);
    run.await.unwrap();

    // The poison message landed on the DLQ stream and was acked out of the
    // group (so it can never block delivery again).
    let dlq_entries = streams
        .range(&format!("{stream}.dlq"), "-", "+")
        .await
        .unwrap();
    assert_eq!(dlq_entries.len(), 1);

    // A follow-up good event must still be processable.
    let good = sanvi_kernel::EventEnvelope::new(
        sanvi_kernel::EventId::try_from(103_i64).unwrap(),
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(203_i64).unwrap(),
        "tenancy.tenant_resumed",
        serde_json::json!({ "slug": "fine" }),
    );
    let payload = serde_json::to_string(&good).unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    let handled = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer_name = format!("broken-test-{}", uuid::Uuid::now_v7());
    let run = tokio::spawn({
        let handled = handled.clone();
        let pool = pool.clone();
        let streams = streams.clone();
        let stream = stream.clone();
        async move {
            StreamConsumer::new(pool, streams)
                .run_until_shutdown(
                    &stream,
                    "broken",
                    &consumer_name,
                    Arc::new(move |_envelope| {
                        let handled = handled.clone();
                        Box::pin(async move {
                            handled.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                    }),
                    shutdown_rx,
                )
                .await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let _ = shutdown_tx.send(true);
    run.await.unwrap();
    assert_eq!(handled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn consumer_metrics_registry_roundtrip() {
    let registry = prometheus::Registry::new();
    let metrics = ConsumerMetrics::new("tenancy");
    metrics.register(&registry).unwrap();
    metrics.lag.set(5);
    metrics.dead_lettered.inc();
    let rendered = prometheus::TextEncoder::new()
        .encode_to_string(&registry.gather())
        .unwrap();
    assert!(rendered.contains("stream_consumer_lag"));
    assert!(rendered.contains("group=\"tenancy\""));
}

/// Reproduces a worker crash: a message is delivered to a consumer (entering
/// its PEL) that then dies before acking — modeled here by reading directly
/// via a raw `XREADGROUP` under a "dead" consumer name and never touching it
/// again, so no handler ever ran and no dedup row was ever written. A
/// replacement process always starts under a *different* consumer name (see
/// `apps/worker`, which mints a fresh UUID per run), so recovery must not
/// rely on re-reading "its own" pending list — it must reclaim entries left
/// by *any* consumer once they've been idle long enough.
#[tokio::test]
async fn crashed_consumers_stale_entries_are_reclaimed_and_processed() {
    let (pool, _db) = prepare_postgres().await;
    let streams = prepare_redis().await;

    let stream = sanvi_testsupport::test_stream("tenancy");
    let group = "tenancy-crash-recovery";
    streams.ensure_consumer_group(&stream, group).await.unwrap();

    let envelope = sanvi_kernel::EventEnvelope::new(
        sanvi_kernel::EventId::try_from(104_i64).unwrap(),
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(204_i64).unwrap(),
        "tenancy.tenant_suspended",
        serde_json::json!({ "slug": "crashed" }),
    );
    let payload = serde_json::to_string(&envelope).unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    // Simulate the crash: a now-dead consumer claims the entry into its PEL
    // (XREADGROUP with ">" ) and never comes back to ack or process it.
    {
        use redis::AsyncCommands;
        let mut conn = streams.connection_manager();
        let options = redis::streams::StreamReadOptions::default()
            .group(group, "dead-consumer")
            .count(1);
        let _: redis::streams::StreamReadReply = conn
            .xread_options(&[stream.as_str()], &[">"], &options)
            .await
            .unwrap();
    }

    // Confirm the dedup table has no record: the "crashed" consumer never
    // got far enough to write one.
    let processed_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform.processed_events WHERE consumer = $1")
            .bind(group)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(processed_before, 0);

    // A replacement consumer, under a brand new name, must still recover and
    // process the entry the dead consumer abandoned.
    let handled = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer_name = format!("replacement-{}", uuid::Uuid::now_v7());
    let run = {
        let handled = handled.clone();
        let pool = pool.clone();
        let streams = streams.clone();
        let group = group.to_string();
        let stream = stream.clone();
        tokio::spawn(async move {
            let consumer = StreamConsumer::new(pool.clone(), streams.clone())
                // Real recovery waits 60s of idle time on a 30s cadence;
                // shrink both so the test doesn't need a real minute-plus.
                .with_recovery_tuning(
                    std::time::Duration::from_millis(50),
                    std::time::Duration::from_millis(50),
                );
            consumer
                .run_until_shutdown(
                    &stream,
                    &group,
                    &consumer_name,
                    Arc::new(move |envelope| {
                        let handled = handled.clone();
                        Box::pin(async move {
                            handled.fetch_add(1, Ordering::SeqCst);
                            let _ = envelope;
                            Ok(())
                        })
                    }),
                    shutdown_rx,
                )
                .await;
        })
    };

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let _ = shutdown_tx.send(true);
    run.await.unwrap();

    assert_eq!(
        handled.load(Ordering::SeqCst),
        1,
        "the replacement consumer must reclaim and run the handler for the abandoned entry"
    );

    let processed_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform.processed_events WHERE consumer = $1")
            .bind(group)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(processed_after, 1);
}
