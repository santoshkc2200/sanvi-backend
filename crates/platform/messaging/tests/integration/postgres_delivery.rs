//! PostgreSQL delivery needs no Redis connection, including replay and fan-out.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use sanvi_messaging::{
    ConsumerHandler, OutboxDispatcher, OutboxWriter, StreamConsumer, StreamName,
    TransactionalConsumerHandler,
};

async fn wait_processed(pool: &sqlx::PgPool, group: &str, expected: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM platform.processed_events WHERE consumer = $1",
            )
            .bind(group)
            .fetch_one(pool)
            .await
            .unwrap();
            if count == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("consumer did not finish");
}

fn counting_handler(count: Arc<AtomicUsize>) -> ConsumerHandler {
    Arc::new(move |_| {
        let count = count.clone();
        Box::pin(async move {
            count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(())
        })
    })
}

#[tokio::test]
async fn postgres_delivery_survives_rollback_restart_and_competing_consumers() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let manager = sanvi_persistence::TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let event = sanvi_kernel::EventEnvelope {
        id: sanvi_kernel::EventId::try_from(101).unwrap(),
        aggregate: "test.aggregate".into(),
        aggregate_id: sanvi_kernel::SnowflakeId::try_from(202).unwrap(),
        event_type: "test.created".into(),
        payload: serde_json::json!({"k":"v"}),
        metadata: serde_json::json!({}),
        occurred_at: chrono::Utc::now(),
    };
    let dispatcher = OutboxDispatcher::postgres(pool.clone(), StreamName::default(), 100, 5);
    let mut tx = manager.begin().await.unwrap();
    OutboxWriter.append(&mut tx, &event).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        dispatcher.run_once().await.unwrap(),
        0,
        "rolled-back state must not publish"
    );
    let mut tx = manager.begin().await.unwrap();
    OutboxWriter.append(&mut tx, &event).await.unwrap();
    assert_eq!(
        dispatcher.run_once().await.unwrap(),
        0,
        "uncommitted events must stay invisible"
    );
    tx.commit().await.unwrap();
    assert_eq!(dispatcher.run_once().await.unwrap(), 1);

    let count = Arc::new(AtomicUsize::new(0));
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let mut tasks = Vec::new();
    // Two workers in one group race on the same event; another group gets its own copy.
    for (group, instance) in [
        ("group-a", "worker-1"),
        ("group-a", "worker-2"),
        ("group-b", "worker-3"),
    ] {
        let consumer = StreamConsumer::postgres(transactions(pool.clone()));
        let handler = counting_handler(count.clone());
        let rx = rx.clone();
        tasks.push(tokio::spawn(async move {
            consumer
                .run_until_shutdown("sanvi.events.test", group, instance, handler, rx)
                .await;
        }));
    }
    wait_processed(&pool, "group-a", 1).await;
    wait_processed(&pool, "group-b", 1).await;
    shutdown.send(true).unwrap();
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(count.load(Ordering::SeqCst), 2);

    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let consumer = StreamConsumer::postgres(transactions(pool.clone()));
    let handler = counting_handler(count.clone());
    let task = tokio::spawn(async move {
        consumer
            .run_until_shutdown("sanvi.events.test", "group-a", "restarted", handler, rx)
            .await;
    });
    tokio::time::sleep(Duration::from_millis(350)).await;
    shutdown.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "restart must preserve inbox deduplication"
    );
}

#[tokio::test]
async fn postgres_consumer_rolls_back_business_effects_and_replays_durable_dead_letters() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    sqlx::query("INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload) VALUES (987, 'test.aggregate', 988, 'test.poison', '{}'::jsonb)")
        .execute(&pool).await.unwrap();
    let dispatcher = OutboxDispatcher::postgres(pool.clone(), StreamName::default(), 100, 5);
    dispatcher.run_once().await.unwrap();
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let consumer = StreamConsumer::postgres(transactions(pool.clone()));
    let handler: TransactionalConsumerHandler = Arc::new(|tx, _| {
        Box::pin(async move {
            sqlx::query("INSERT INTO platform.scheduled_jobs (scheduler_key, name) VALUES ('effect', 'rolled-back')")
            .execute(&mut **tx).await?;
            anyhow::bail!("poison event")
        })
    });
    let task = tokio::spawn(async move {
        consumer
            .run_transactional_until_shutdown("sanvi.events.test", "poison", "worker", handler, rx)
            .await;
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !sanvi_messaging::list_consumer_dead_letters(&pool, 10)
                .await
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    shutdown.send(true).unwrap();
    task.await.unwrap();
    let effects: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.scheduled_jobs WHERE scheduler_key = 'effect'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(effects, 0, "failed handler mutations must roll back");
    let processed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.processed_events WHERE consumer = 'poison'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        processed, 0,
        "failed handler must not commit inbox acknowledgement"
    );

    let manager =
        sanvi_persistence::TxManager::new(db.admin_pool().await, sanvi_testsupport::test_ids());
    let mut scope = manager
        .begin_unscoped("postgres_replay", "operator")
        .await
        .unwrap();
    assert!(
        sanvi_messaging::replay_consumer_dead_letter(&mut scope, "poison", 987)
            .await
            .unwrap()
    );
    scope.commit().await.unwrap();
    assert_eq!(dispatcher.run_once().await.unwrap(), 1);
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let consumer = StreamConsumer::postgres(transactions(pool.clone()));
    let handler: TransactionalConsumerHandler = Arc::new(|tx, _| {
        Box::pin(async move {
            sqlx::query("INSERT INTO platform.scheduled_jobs (scheduler_key, name) VALUES ('effect', 'committed')")
            .execute(&mut **tx).await?;
            Ok(())
        })
    });
    let task = tokio::spawn(async move {
        consumer
            .run_transactional_until_shutdown(
                "sanvi.events.test",
                "poison",
                "restarted",
                handler,
                rx,
            )
            .await;
    });
    wait_processed(&pool, "poison", 1).await;
    shutdown.send(true).unwrap();
    task.await.unwrap();
    let effects: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.scheduled_jobs WHERE scheduler_key = 'effect'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(effects, 1);
}

struct UnavailableBroker;

impl sanvi_messaging::EventPublisher for UnavailableBroker {
    fn publish<'a>(
        &'a self,
        _: &'a str,
        _: &'a sanvi_kernel::EventEnvelope,
    ) -> sanvi_messaging::PublishFuture<'a> {
        Box::pin(async { anyhow::bail!("broker unavailable") })
    }
}

#[tokio::test]
async fn broker_adapter_failure_never_marks_publication_and_dead_letters_after_budget() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    sqlx::query("INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload) VALUES (777, 'test.aggregate', 778, 'test.broker', '{}'::jsonb)")
        .execute(&pool).await.unwrap();
    let dispatcher = OutboxDispatcher::new(
        pool.clone(),
        UnavailableBroker,
        StreamName::default(),
        10,
        2,
    );
    assert_eq!(dispatcher.run_once().await.unwrap(), 0);
    let state: (Option<chrono::DateTime<chrono::Utc>>, i32) =
        sqlx::query_as("SELECT published_at, attempts FROM platform.outbox WHERE id = 777")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, (None, 1));
    assert_eq!(dispatcher.run_once().await.unwrap(), 0);
    assert_eq!(dispatcher.pending_count().await.unwrap(), 0);
    let parked = sanvi_messaging::list_dead_letters(&db.admin_pool().await, 10)
        .await
        .unwrap();
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].id, 777);
    assert_eq!(parked[0].failure, "broker unavailable");
}

fn transactions(pool: sqlx::PgPool) -> sanvi_persistence::TxManager {
    sanvi_persistence::TxManager::new(pool, sanvi_testsupport::test_ids())
}
