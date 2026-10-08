//! Integration tests: real Postgres and Redis. Run with `just test`
//! (docker must be available).

use sanvi_messaging::{OutboxDispatcher, RedisConfig, StreamName, StreamsClient, connect_redis};
use sanvi_testsupport::TestDb;

async fn prepare_postgres() -> (sanvi_persistence::PgPool, TestDb) {
    let db = sanvi_testsupport::test_db().await;
    (db.app_pool(), db)
}

#[tokio::test]
async fn dispatcher_relays_outbox_rows_to_the_stream() {
    let (pool, _db) = prepare_postgres().await;

    let streams = StreamsClient::new(
        connect_redis(&RedisConfig {
            url: sanvi_testsupport::redis_url().await,
            ..RedisConfig::default()
        })
        .await
        .unwrap(),
    );

    // Insert an unpublished domain event.
    let event_id = sanvi_kernel::EventId::try_from(101_i64).unwrap();
    sqlx::query(
        "INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload)
         VALUES ($1, 'test.aggregate', $2, 'test.something_happened', $3::jsonb)",
    )
    .bind(event_id.as_i64())
    .bind(202_i64)
    .bind(r#"{"k":"v"}"#)
    .execute(&pool)
    .await
    .unwrap();

    let stream = StreamName(sanvi_testsupport::test_stream("events"));
    let dispatcher = OutboxDispatcher::new(pool.clone(), streams.clone(), stream.clone(), 50, 5);

    let published = dispatcher.run_once().await.expect("dispatch");
    assert_eq!(published, 1);

    // The row is marked published…
    let published_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT published_at FROM platform.outbox WHERE id = $1")
            .bind(event_id.as_i64())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(published_at.is_some());

    // …and the event reached the per-context stream (`{base}.{aggregate
    // context}`) with the same id as the outbox row.
    let entries = streams
        .range(&stream.stream_for("test.aggregate"), "-", "+")
        .await
        .unwrap();
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
    let (pool, _db) = prepare_postgres().await;

    let streams = StreamsClient::new(
        connect_redis(&RedisConfig {
            url: sanvi_testsupport::redis_url().await,
            ..RedisConfig::default()
        })
        .await
        .unwrap(),
    );

    const ROWS: usize = 20;
    let mut event_ids = Vec::with_capacity(ROWS);
    for _ in 0..ROWS {
        let event_id = sanvi_kernel::EventId::try_from(1_000_i64 + event_ids.len() as i64).unwrap();
        sqlx::query(
            "INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload)
             VALUES ($1, 'test.aggregate', $2, 'test.something_happened', $3::jsonb)",
        )
        .bind(event_id.as_i64())
        .bind(2_000_i64 + event_ids.len() as i64)
        .bind(r#"{"k":"v"}"#)
        .execute(&pool)
        .await
        .unwrap();
        event_ids.push(event_id);
    }

    let stream = StreamName(sanvi_testsupport::test_stream("concurrent"));
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
        .range(&stream.stream_for("test.aggregate"), "-", "+")
        .await
        .unwrap();
    assert_eq!(
        entries.len(),
        ROWS,
        "the stream must contain exactly one entry per row, no duplicates"
    );

    let published_ids: std::collections::HashSet<sanvi_kernel::EventId> = entries
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
    let streams = StreamsClient::new(
        connect_redis(&RedisConfig {
            url: sanvi_testsupport::redis_url().await,
            ..RedisConfig::default()
        })
        .await
        .unwrap(),
    );

    let stream = sanvi_testsupport::test_stream("groups");
    streams
        .ensure_consumer_group(&stream, "consumer-a")
        .await
        .unwrap();
    streams
        .ensure_consumer_group(&stream, "consumer-a")
        .await
        .unwrap();
    streams
        .ensure_consumer_group(&stream, "consumer-b")
        .await
        .unwrap();
}

/// TASK-023 step 7 (FR-1112): dead-lettered outbox rows are listed for
/// triage and replayable — replay moves the row back to the outbox with a
/// fresh attempt budget, and replaying what is not dead-lettered reports
/// false instead of fabricating work.
#[tokio::test]
async fn dead_lettered_rows_list_and_replay() {
    let (pool, db) = prepare_postgres().await;
    // The app role holds INSERT-only on the DLQ table by design; triage
    // and replay run privileged (the CLI connects as migrator), so the
    // operator-level reads and the replay scope use the admin pool here.
    let admin = db.admin_pool().await;

    sqlx::query(
        "INSERT INTO platform.outbox_dlq
             (id, aggregate, aggregate_id, event_type, payload, metadata, occurred_at, attempts, failure)
         VALUES (424242, 'tenants', 42, 'tenants.tenant_created', '{}', '{}', now(), 5, 'stream down')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let listed = sanvi_messaging::list_dead_letters(&admin, 20)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, 424242);
    assert_eq!(listed[0].event_type, "tenants.tenant_created");
    assert_eq!(listed[0].failure, "stream down");

    let tx = sanvi_persistence::TxManager::new(admin.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx
        .begin_unscoped("matrix_dlq_replay", "platform-admin")
        .await
        .unwrap();
    assert!(
        sanvi_messaging::replay_dead_letter(&mut scope, 424242)
            .await
            .unwrap()
    );
    // Nothing left to replay: the second call reports false.
    assert!(
        !sanvi_messaging::replay_dead_letter(&mut scope, 424242)
            .await
            .unwrap()
    );
    scope.commit().await.unwrap();

    // The row is back in the outbox, unpublished, with a fresh budget —
    // the next dispatch poll picks it up again.
    let row: (Option<chrono::DateTime<chrono::Utc>>, i32) =
        sqlx::query_as("SELECT published_at, attempts FROM platform.outbox WHERE id = 424242")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.0.is_none());
    assert_eq!(row.1, 0);
    let remaining: (i64,) =
        sqlx::query_as("SELECT count(*) FROM platform.outbox_dlq WHERE id = 424242")
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(remaining.0, 0);
}
