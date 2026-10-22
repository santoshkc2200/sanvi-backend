//! Integration tests: stream consumers against a real Postgres and Redis.
//! Run with `just test` (docker must be available).

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use sanvi_messaging::{
    ConsumerMetrics, HandlerError, RedisConfig, StreamConsumer, StreamsClient,
    TransactionalConsumerHandler, connect_redis,
};
use sanvi_testsupport::TestDb;

async fn prepare_postgres() -> (sanvi_persistence::PgPool, TestDb) {
    let db = sanvi_testsupport::test_db().await;
    (db.app_pool(), db)
}

#[tokio::test]
async fn retained_postgres_event_recovers_when_redis_has_lost_the_stream() {
    let (pool, _db) = prepare_postgres().await;
    let streams = prepare_redis().await;
    let stream = format!("{}.test", sanvi_testsupport::test_stream("redis-loss"));
    sqlx::query("INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload, published_at) VALUES (765, 'test.aggregate', 766, 'test.recovery', '{}'::jsonb, now())")
        .execute(&pool).await.unwrap();
    let handled = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer = StreamConsumer::new(pool, streams);
    let count = handled.clone();
    let task = tokio::spawn(async move {
        consumer
            .run_until_shutdown(
                &stream,
                "redis-loss",
                "recovered",
                Arc::new(move |_| {
                    let count = count.clone();
                    Box::pin(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                }),
                shutdown_rx,
            )
            .await;
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while handled.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    let _ = shutdown_tx.send(true);
    task.await.unwrap();
    assert!(
        result.is_ok(),
        "published PostgreSQL events must survive Redis data loss"
    );
    assert_eq!(handled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn consumer_failure_survives_redis_loss_and_replays_from_the_audited_ledger() {
    use redis::AsyncCommands;
    let (pool, db) = prepare_postgres().await;
    let streams = prepare_redis().await;
    let base = sanvi_messaging::StreamName(sanvi_testsupport::test_stream("durable-dlq"));
    let stream = base.stream_for("test.aggregate");
    sqlx::query("INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload, published_at) VALUES (987, 'test.aggregate', 988, 'test.park', '{}'::jsonb, now())")
        .execute(&pool).await.unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let stream_for_task = stream.clone();
    let task = tokio::spawn(async move {
        consumer
            .run_until_shutdown(
                &stream_for_task,
                "durable-dlq",
                "failed",
                Arc::new(|_| Box::pin(async { anyhow::bail!("poison event") })),
                rx,
            )
            .await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !sanvi_messaging::list_consumer_dead_letters(&pool, 20)
                .await
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tx.send(true).unwrap();
    task.await.unwrap();
    let _: usize = streams
        .connection_manager()
        .del(&[stream.clone(), format!("{stream}.dlq")])
        .await
        .unwrap();
    let rows = sanvi_messaging::list_consumer_dead_letters(&pool, 20)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_id, Some(987));

    let manager =
        sanvi_persistence::TxManager::new(db.admin_pool().await, sanvi_testsupport::test_ids());
    // An interrupted replay preserves both parked data and publication state.
    let mut scope = manager
        .begin_unscoped("test_failed_replay", "operator")
        .await
        .unwrap();
    assert!(
        sanvi_messaging::replay_consumer_dead_letter(&mut scope, "durable-dlq", 987)
            .await
            .unwrap()
    );
    scope.rollback().await.unwrap();
    assert_eq!(
        sanvi_messaging::list_consumer_dead_letters(&pool, 20)
            .await
            .unwrap()
            .len(),
        1
    );
    let mut scope = manager
        .begin_unscoped("test_replay", "operator")
        .await
        .unwrap();
    assert!(
        sanvi_messaging::replay_consumer_dead_letter(&mut scope, "durable-dlq", 987)
            .await
            .unwrap()
    );
    scope.commit().await.unwrap();
    let dispatcher =
        sanvi_messaging::OutboxDispatcher::new(pool.clone(), streams.clone(), base, 10, 5);
    assert_eq!(dispatcher.run_once().await.unwrap(), 1);
    let (tx, rx) = tokio::sync::watch::channel(false);
    let count = Arc::new(AtomicUsize::new(0));
    let effect = count.clone();
    let task = tokio::spawn(async move {
        StreamConsumer::new(pool, streams)
            .run_until_shutdown(
                &stream,
                "durable-dlq",
                "repaired",
                Arc::new(move |_| {
                    let effect = effect.clone();
                    Box::pin(async move {
                        effect.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                }),
                rx,
            )
            .await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while count.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tx.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
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

#[tokio::test]
async fn crash_between_database_effect_and_dedup_row_leaves_no_partial_state() {
    let (pool, db) = prepare_postgres().await;
    let streams = prepare_redis().await;

    let admin = db.admin_pool().await;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS public.test_consumer_effects (
            id bigint PRIMARY KEY,
            val text NOT NULL
        )",
    )
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query("GRANT ALL ON public.test_consumer_effects TO sanvi_app")
        .execute(&admin)
        .await
        .unwrap();

    let stream = sanvi_testsupport::test_stream("atomic_tx");
    let group = "atomic-tx-group";
    let event_id = sanvi_kernel::EventId::try_from(201_i64).unwrap();
    let envelope = sanvi_kernel::EventEnvelope::new(
        event_id,
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(301_i64).unwrap(),
        "tenancy.tenant_created",
        serde_json::json!({ "name": "Atomic Corp" }),
    );
    let payload = serde_json::to_string(&envelope).unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    // In a transactional consumer, business writes occur on &mut tx.
    // If a crash occurs before commit (simulated here by returning an error on first attempt),
    // the transaction rolls back both the business write and the dedup insert.
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();

    let transactional_handler: TransactionalConsumerHandler = Arc::new(move |tx, envelope| {
        let attempts = attempts_clone.clone();
        Box::pin(async move {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            // Write business change to database
            sqlx::query("INSERT INTO public.test_consumer_effects (id, val) VALUES ($1, $2)")
                .bind(envelope.id.as_i64())
                .bind("business-data")
                .execute(&mut **tx)
                .await
                .unwrap();

            if attempt == 0 {
                // Simulate crash/error before commit
                return Err(HandlerError::transient(anyhow::anyhow!("crash before commit")).into());
            }

            Ok(())
        })
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer_name = format!("tx-consumer-{}", uuid::Uuid::now_v7());
    let consumer = StreamConsumer::new(pool.clone(), streams.clone()).with_retry_tuning(
        3,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(100),
    );

    let run = tokio::spawn({
        let stream = stream.clone();
        let consumer_name = consumer_name.clone();
        async move {
            consumer
                .run_transactional_until_shutdown(
                    &stream,
                    group,
                    &consumer_name,
                    transactional_handler,
                    shutdown_rx,
                )
                .await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let _ = shutdown_tx.send(true);
    run.await.unwrap();

    // The handler was retried:
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    // Business effects committed EXACTLY ONCE (not duplicated despite crash on attempt 0):
    let effect_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM public.test_consumer_effects WHERE id = $1")
            .bind(event_id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(effect_count, 1);

    // Dedup row recorded:
    let processed_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.processed_events WHERE consumer = $1 AND event_id = $2",
    )
    .bind(group)
    .bind(event_id.as_i64())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(processed_count, 1);
}

#[tokio::test]
async fn concurrent_delivery_of_same_event_executes_database_handler_exactly_once() {
    let (pool, db) = prepare_postgres().await;
    let streams = prepare_redis().await;

    let admin = db.admin_pool().await;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS public.test_concurrent_counter (
            id bigint PRIMARY KEY,
            count int NOT NULL
        )",
    )
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query("GRANT ALL ON public.test_concurrent_counter TO sanvi_app")
        .execute(&admin)
        .await
        .unwrap();

    let stream = sanvi_testsupport::test_stream("concurrent_delivery");
    let group = "concurrent-group";
    streams.ensure_consumer_group(&stream, group).await.unwrap();

    let event_id = sanvi_kernel::EventId::try_from(202_i64).unwrap();
    sqlx::query(
        "INSERT INTO public.test_concurrent_counter (id, count) VALUES ($1, 0)
         ON CONFLICT (id) DO UPDATE SET count = 0",
    )
    .bind(event_id.as_i64())
    .execute(&pool)
    .await
    .unwrap();

    let envelope = sanvi_kernel::EventEnvelope::new(
        event_id,
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(302_i64).unwrap(),
        "tenancy.tenant_updated",
        serde_json::json!({ "name": "Concurrent Inc" }),
    );
    let payload = serde_json::to_string(&envelope).unwrap();

    // Deliver the exact same event twice to the stream
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    let execution_count = Arc::new(AtomicUsize::new(0));

    let make_handler = |exec: Arc<AtomicUsize>| -> TransactionalConsumerHandler {
        Arc::new(move |tx, envelope| {
            let exec = exec.clone();
            Box::pin(async move {
                exec.fetch_add(1, Ordering::SeqCst);
                sqlx::query(
                    "UPDATE public.test_concurrent_counter SET count = count + 1 WHERE id = $1",
                )
                .bind(envelope.id.as_i64())
                .execute(&mut **tx)
                .await
                .unwrap();
                Ok(())
            })
        })
    };

    let (shutdown1_tx, shutdown1_rx) = tokio::sync::watch::channel(false);
    let (shutdown2_tx, shutdown2_rx) = tokio::sync::watch::channel(false);

    let consumer_name_1 = format!("concurrent-c1-{}", uuid::Uuid::now_v7());
    let consumer_name_2 = format!("concurrent-c2-{}", uuid::Uuid::now_v7());

    let c1 = StreamConsumer::new(pool.clone(), streams.clone());
    let c2 = StreamConsumer::new(pool.clone(), streams.clone());

    let run1 = tokio::spawn({
        let stream = stream.clone();
        let h1 = make_handler(execution_count.clone());
        async move {
            c1.run_transactional_until_shutdown(&stream, group, &consumer_name_1, h1, shutdown1_rx)
                .await;
        }
    });

    let run2 = tokio::spawn({
        let stream = stream.clone();
        let h2 = make_handler(execution_count.clone());
        async move {
            c2.run_transactional_until_shutdown(&stream, group, &consumer_name_2, h2, shutdown2_rx)
                .await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let _ = shutdown1_tx.send(true);
    let _ = shutdown2_tx.send(true);
    let _ = tokio::join!(run1, run2);

    // Only one execution actually ran:
    assert_eq!(execution_count.load(Ordering::SeqCst), 1);

    // Business state counter incremented exactly once:
    let final_count: i32 =
        sqlx::query_scalar("SELECT count FROM public.test_concurrent_counter WHERE id = $1")
            .bind(event_id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(final_count, 1);

    // Processed events dedup row recorded exactly once:
    let dedup_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.processed_events WHERE consumer = $1 AND event_id = $2",
    )
    .bind(group)
    .bind(event_id.as_i64())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dedup_count, 1);
}

#[tokio::test]
async fn failing_dead_letter_append_does_not_ack_event() {
    let (pool, _db) = prepare_postgres().await;
    let streams = prepare_redis().await;

    let stream = sanvi_testsupport::test_stream("fail_dlq");
    let group = "fail-dlq-group";
    streams.ensure_consumer_group(&stream, group).await.unwrap();

    let event_id = sanvi_kernel::EventId::try_from(203_i64).unwrap();
    let envelope = sanvi_kernel::EventEnvelope::new(
        event_id,
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(303_i64).unwrap(),
        "tenancy.tenant_failed",
        serde_json::json!({ "slug": "doomed" }),
    );
    let payload = serde_json::to_string(&envelope).unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    // Corrupt the DLQ stream key in Redis by setting it as a string so XADD fails with WRONGTYPE
    let dlq_stream = format!("{stream}.dlq");
    {
        let mut conn = streams.connection_manager();
        redis::cmd("SET")
            .arg(&dlq_stream)
            .arg("corrupted-not-a-stream")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer_name = format!("fail-dlq-c-{}", uuid::Uuid::now_v7());
    let consumer = StreamConsumer::new(pool.clone(), streams.clone()).with_retry_tuning(
        1,
        std::time::Duration::from_millis(10),
        std::time::Duration::from_millis(10),
    );

    let run = tokio::spawn({
        let stream = stream.clone();
        async move {
            consumer
                .run_until_shutdown(
                    &stream,
                    group,
                    &consumer_name,
                    Arc::new(move |_envelope| {
                        Box::pin(async move {
                            // Permanent error: triggers immediate dead-lettering
                            anyhow::bail!("unrecoverable failure");
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

    // Because the dead-letter append failed, the consumer MUST NOT have acked the event!
    // The entry must still be in the group's pending entries list (PEL).
    {
        use redis::AsyncCommands;
        let mut conn = streams.connection_manager();
        let pending: redis::streams::StreamPendingReply =
            conn.xpending(&stream, group).await.unwrap();
        assert_eq!(
            pending.count(),
            1,
            "failing DLQ append must NOT ack the original event — it must remain pending"
        );
    }
}

#[tokio::test]
async fn transient_handler_failure_is_retried_with_backoff_and_succeeds() {
    let (pool, _db) = prepare_postgres().await;
    let streams = prepare_redis().await;

    let stream = sanvi_testsupport::test_stream("transient_retry");
    let group = "transient-retry-group";
    streams.ensure_consumer_group(&stream, group).await.unwrap();

    let event_id = sanvi_kernel::EventId::try_from(204_i64).unwrap();
    let envelope = sanvi_kernel::EventEnvelope::new(
        event_id,
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(304_i64).unwrap(),
        "tenancy.tenant_transient",
        serde_json::json!({ "slug": "transient" }),
    );
    let payload = serde_json::to_string(&envelope).unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer_name = format!("transient-c-{}", uuid::Uuid::now_v7());
    let consumer = StreamConsumer::new(pool.clone(), streams.clone()).with_retry_tuning(
        3,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(100),
    );

    let run = tokio::spawn({
        let stream = stream.clone();
        async move {
            consumer
                .run_until_shutdown(
                    &stream,
                    group,
                    &consumer_name,
                    Arc::new(move |_envelope| {
                        let attempts = attempts_clone.clone();
                        Box::pin(async move {
                            let att = attempts.fetch_add(1, Ordering::SeqCst);
                            if att < 2 {
                                // Fail transiently twice
                                return Err(HandlerError::transient(anyhow::anyhow!(
                                    "transient error"
                                ))
                                .into());
                            }
                            // Succeed on attempt 3
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

    // Verified: took 3 attempts (retried twice with backoff)
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    // Dedup row exists (processed successfully):
    let processed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.processed_events WHERE consumer = $1 AND event_id = $2",
    )
    .bind(group)
    .bind(event_id.as_i64())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(processed, 1);

    // DLQ was NOT populated:
    let dlq_entries = streams
        .range(&format!("{stream}.dlq"), "-", "+")
        .await
        .unwrap();
    assert_eq!(dlq_entries.len(), 0);
}

#[tokio::test]
async fn transient_handler_failure_dead_letters_when_retries_exhausted() {
    let (pool, _db) = prepare_postgres().await;
    let streams = prepare_redis().await;

    let stream = sanvi_testsupport::test_stream("exhausted_retry");
    let group = "exhausted-retry-group";
    streams.ensure_consumer_group(&stream, group).await.unwrap();

    let event_id = sanvi_kernel::EventId::try_from(205_i64).unwrap();
    let envelope = sanvi_kernel::EventEnvelope::new(
        event_id,
        "tenancy.tenant",
        sanvi_kernel::SnowflakeId::try_from(305_i64).unwrap(),
        "tenancy.tenant_exhausted",
        serde_json::json!({ "slug": "exhausted" }),
    );
    let payload = serde_json::to_string(&envelope).unwrap();
    streams
        .append(&stream, &[("event", payload.as_str())], 100)
        .await
        .unwrap();

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let consumer_name = format!("exhausted-c-{}", uuid::Uuid::now_v7());
    let consumer = StreamConsumer::new(pool.clone(), streams.clone()).with_retry_tuning(
        2,
        std::time::Duration::from_millis(30),
        std::time::Duration::from_millis(50),
    );

    let run = tokio::spawn({
        let stream = stream.clone();
        async move {
            consumer
                .run_until_shutdown(
                    &stream,
                    group,
                    &consumer_name,
                    Arc::new(move |_envelope| {
                        let attempts = attempts_clone.clone();
                        Box::pin(async move {
                            attempts.fetch_add(1, Ordering::SeqCst);
                            Err(HandlerError::transient(anyhow::anyhow!("always transient")).into())
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

    // 1 initial attempt + 2 retries = 3 total attempts
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    // Lands on DLQ when retries are exhausted:
    let dlq_entries = streams
        .range(&format!("{stream}.dlq"), "-", "+")
        .await
        .unwrap();
    assert_eq!(dlq_entries.len(), 1);

    // Acked out of group:
    {
        use redis::AsyncCommands;
        let mut conn = streams.connection_manager();
        let pending: redis::streams::StreamPendingReply =
            conn.xpending(&stream, group).await.unwrap();
        assert_eq!(pending.count(), 0);
    }
}
