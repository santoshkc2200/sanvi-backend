use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use sanvi_messaging::{FnJob, Scheduler};

#[tokio::test]
async fn job_runs_shortly_after_startup_and_repeatedly_on_subsequent_ticks() {
    let db = sanvi_testsupport::test_db().await;
    let conn = db.app_pool();
    let lock_key = sanvi_testsupport::test_stream("scheduler_startup");
    let runs = Arc::new(AtomicUsize::new(0));
    let runs_clone = runs.clone();

    let job = Arc::new(FnJob::new("sweep", Duration::from_millis(40), move || {
        let runs = runs_clone.clone();
        Box::pin(async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }));

    let scheduler = Scheduler::new(transactions(conn), "worker-startup", vec![job])
        .with_lock_key(lock_key)
        .with_tick(Duration::from_millis(30))
        .with_lock_ttl(5);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        scheduler.run_until_shutdown(shutdown_rx).await;
    });

    // 1. Job runs shortly after startup
    tokio::time::timeout(Duration::from_secs(5), async {
        while runs.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job should run shortly after startup");

    // 2. Job runs repeatedly on subsequent ticks
    tokio::time::timeout(Duration::from_secs(5), async {
        while runs.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job should run repeatedly on subsequent ticks");

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
}

#[tokio::test]
async fn scheduler_stops_on_shutdown() {
    let db = sanvi_testsupport::test_db().await;
    let conn = db.app_pool();
    let lock_key = sanvi_testsupport::test_stream("scheduler_shutdown");
    let runs = Arc::new(AtomicUsize::new(0));
    let runs_clone = runs.clone();

    let job = Arc::new(FnJob::new("sweep", Duration::from_millis(20), move || {
        let runs = runs_clone.clone();
        Box::pin(async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }));

    let scheduler = Scheduler::new(transactions(conn), "worker-shutdown", vec![job])
        .with_lock_key(lock_key)
        .with_tick(Duration::from_millis(20))
        .with_lock_ttl(5);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        scheduler.run_until_shutdown(shutdown_rx).await;
    });

    // Wait until it has run at least once
    tokio::time::timeout(Duration::from_secs(5), async {
        while runs.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // Trigger shutdown and wait for the scheduler task to exit
    let _ = shutdown_tx.send(true);
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("scheduler task must exit promptly on shutdown")
        .expect("scheduler task join error");

    let count_after_shutdown = runs.load(Ordering::SeqCst);
    // Wait several tick intervals to ensure no subsequent ticks run
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        count_after_shutdown,
        "job must not run after scheduler is shut down"
    );
}

#[tokio::test]
async fn scheduler_does_not_run_on_two_workers_at_once() {
    let db = sanvi_testsupport::test_db().await;
    let conn1 = db.app_pool();
    let conn2 = conn1.clone();
    let lock_key = sanvi_testsupport::test_stream("scheduler_leader_excl");

    let w1_runs = Arc::new(AtomicUsize::new(0));
    let w1_runs_clone = w1_runs.clone();
    let job1 = Arc::new(FnJob::new("sweep", Duration::from_millis(30), move || {
        let w1 = w1_runs_clone.clone();
        Box::pin(async move {
            w1.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }));

    let w2_runs = Arc::new(AtomicUsize::new(0));
    let w2_runs_clone = w2_runs.clone();
    let job2 = Arc::new(FnJob::new("sweep", Duration::from_millis(30), move || {
        let w2 = w2_runs_clone.clone();
        Box::pin(async move {
            w2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }));

    let s1 = Scheduler::new(transactions(conn1), "worker-1", vec![job1])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(30))
        .with_lock_ttl(5);

    let s2 = Scheduler::new(transactions(conn2), "worker-2", vec![job2])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(30))
        .with_lock_ttl(5);

    let (shutdown1_tx, shutdown1_rx) = tokio::sync::watch::channel(false);
    let (shutdown2_tx, shutdown2_rx) = tokio::sync::watch::channel(false);

    let h1 = tokio::spawn(async move {
        s1.run_until_shutdown(shutdown1_rx).await;
    });

    // Wait for worker 1 to acquire the lock and run
    tokio::time::timeout(Duration::from_secs(5), async {
        while w1_runs.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // Now start worker 2 with the same lock key
    let h2 = tokio::spawn(async move {
        s2.run_until_shutdown(shutdown2_rx).await;
    });

    // Let both run concurrently for several ticks
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Worker 1 should have run multiple times, Worker 2 should have run 0 times
    assert!(
        w1_runs.load(Ordering::SeqCst) >= 2,
        "leader worker 1 must run jobs"
    );
    assert_eq!(
        w2_runs.load(Ordering::SeqCst),
        0,
        "follower worker 2 must not run jobs while worker 1 holds leadership"
    );

    // Now gracefully shut down worker 1
    let _ = shutdown1_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), h1).await;

    // Worker 2 should now take over leadership and run
    tokio::time::timeout(Duration::from_secs(5), async {
        while w2_runs.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker 2 should take over leadership after worker 1 shuts down");

    let _ = shutdown2_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), h2).await;
}

#[tokio::test]
async fn scheduler_renews_lease_during_long_job_sequence() {
    let db = sanvi_testsupport::test_db().await;
    let conn = db.app_pool();
    let lock_key = sanvi_testsupport::test_stream("scheduler_renew_seq");

    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = completed.clone();

    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();
    // The job outlives the one-second lease and requires repeated renewal.
    let job = Arc::new(FnJob::new("long_job", Duration::from_secs(10), move || {
        let completed = completed_clone.clone();
        let started = started_clone.clone();
        Box::pin(async move {
            started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(2400)).await;
            completed.store(true, Ordering::SeqCst);
            Ok(())
        })
    }));

    let s1 = Scheduler::new(transactions(conn.clone()), "worker-long", vec![job])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(100))
        .with_lock_ttl(1);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let h1 = tokio::spawn(async move {
        s1.run_until_shutdown(shutdown_rx).await;
    });

    // Competing scheduler on the same lock key trying to acquire
    let s2 = Scheduler::new(transactions(conn), "worker-challenger", vec![])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(100))
        .with_lock_ttl(1);

    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // More than the original TTL has elapsed while the job is still running.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s2_acquired = s2.acquire_or_renew().await.unwrap();
    assert!(
        !s2_acquired,
        "challenger must not acquire lock because worker-long's keepalive renewed it"
    );

    // Wait for the job to complete
    tokio::time::timeout(Duration::from_secs(5), async {
        while !completed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("long job should complete successfully");

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), h1).await;
}

#[tokio::test]
async fn scheduler_cancels_job_when_lease_is_lost() {
    let db = sanvi_testsupport::test_db().await;
    let conn = db.app_pool();
    let lock_key = sanvi_testsupport::test_stream("scheduler_lost_lease");

    struct CancelProbe(Arc<AtomicBool>);
    impl Drop for CancelProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_clone = cancelled.clone();
    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();

    // Only dropping the running future marks cancellation.
    let job = Arc::new(FnJob::new("long_job", Duration::from_secs(10), move || {
        let cancelled = cancelled_clone.clone();
        let started = started_clone.clone();
        Box::pin(async move {
            let _probe = CancelProbe(cancelled);
            started.store(true, Ordering::SeqCst);
            std::future::pending().await
        })
    }));

    let s1 = Scheduler::new(transactions(conn.clone()), "worker-evicted", vec![job])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(50))
        .with_lock_ttl(1);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let h1 = tokio::spawn(async move {
        s1.run_until_shutdown(shutdown_rx).await;
    });

    // Wait until the job has started
    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // Simulate lease loss while a job is running.
    sqlx::query("UPDATE platform.worker_leases SET owner = 'stealer' WHERE name = $1")
        .bind(&lock_key)
        .execute(&conn)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while !cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("running job must be dropped when leadership is lost");

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), h1).await;
}

/// TASK-023 step 7 (FR-1112): a permanently failing job stops at its retry
/// budget instead of burning capacity forever — two failures suspend it,
/// and the long backoff keeps it suspended for the rest of the run.
#[tokio::test]
async fn permanently_failing_job_stops_at_its_retry_budget() {
    let db = sanvi_testsupport::test_db().await;
    let conn = db.app_pool();
    let lock_key = sanvi_testsupport::test_stream("scheduler_budget");
    let runs = Arc::new(AtomicUsize::new(0));
    let runs_clone = runs.clone();

    let job = Arc::new(FnJob::new(
        "always_fails",
        Duration::from_millis(40),
        move || {
            let runs = runs_clone.clone();
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("permanent outage"))
            })
        },
    ));

    let scheduler = Scheduler::new(transactions(conn), "worker-budget", vec![job])
        .with_lock_key(lock_key)
        .with_tick(Duration::from_millis(20))
        .with_lock_ttl(5)
        .with_retry_policy(sanvi_resilience::JobRetryPolicy {
            max_consecutive_failures: 2,
            base_backoff_secs: 3600,
            max_backoff_secs: 3600,
        });

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        scheduler.run_until_shutdown(shutdown_rx).await;
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        while runs.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("scheduler must reach its failure budget");
    // Observe several normal intervals to prove further runs are suspended.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "a permanently failing job must stop at its retry budget"
    );
}

#[tokio::test]
async fn successful_schedule_does_not_run_again_after_restart() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let runs = Arc::new(AtomicUsize::new(0));
    for instance in ["first", "restarted"] {
        let count = runs.clone();
        let job = Arc::new(FnJob::new("hourly", Duration::from_secs(3600), move || {
            let count = count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }));
        let scheduler = Scheduler::new(transactions(pool.clone()), instance, vec![job])
            .with_tick(Duration::from_millis(20));
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            scheduler.run_until_shutdown(rx).await;
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let finished: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM platform.scheduled_jobs WHERE name = 'hourly' AND last_finished_at IS NOT NULL)")
                    .fetch_one(&pool).await.unwrap();
                if finished { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        shutdown.send(true).unwrap();
        task.await.unwrap();
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "restart must preserve the persisted due time"
    );
}

#[tokio::test]
async fn retry_budget_and_error_survive_scheduler_restart() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let runs = Arc::new(AtomicUsize::new(0));
    for instance in ["first", "restarted"] {
        let count = runs.clone();
        let job = Arc::new(FnJob::new("fails", Duration::from_millis(10), move || {
            let count = count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("persistent outage")
            })
        }));
        let scheduler = Scheduler::new(transactions(pool.clone()), instance, vec![job])
            .with_tick(Duration::from_millis(10))
            .with_retry_policy(sanvi_resilience::JobRetryPolicy {
                max_consecutive_failures: 2,
                base_backoff_secs: 3600,
                max_backoff_secs: 3600,
            });
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            scheduler.run_until_shutdown(rx).await;
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let failures: Option<i32> = sqlx::query_scalar(
                    "SELECT consecutive_failures FROM platform.scheduled_jobs WHERE name = 'fails'",
                )
                .fetch_optional(&pool)
                .await
                .unwrap();
                if failures == Some(2) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        shutdown.send(true).unwrap();
        task.await.unwrap();
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "restart must not reset failure budget"
    );
    let failure: String =
        sqlx::query_scalar("SELECT last_error FROM platform.scheduled_jobs WHERE name = 'fails'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(failure, "persistent outage");
}

#[tokio::test]
async fn expired_worker_lease_can_be_claimed_and_stale_owner_cannot_release_it() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let first = Scheduler::new(transactions(pool.clone()), "dead-worker", vec![]);
    let second = Scheduler::new(transactions(pool.clone()), "replacement", vec![]);
    assert!(first.acquire_or_renew().await.unwrap());
    assert!(!second.acquire_or_renew().await.unwrap());
    sqlx::query(
        "UPDATE platform.worker_leases SET expires_at = clock_timestamp() - interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(second.acquire_or_renew().await.unwrap());
    assert!(!first.release_lock().await.unwrap());
    assert!(!first.acquire_or_renew().await.unwrap());
}

#[tokio::test]
async fn jobs_migration_is_reversible_without_losing_outbox_events() {
    let db = sanvi_testsupport::test_db().await;
    let admin = db.admin_pool().await;
    sqlx::query("INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload) VALUES (456, 'test.aggregate', 457, 'test.retained', '{}'::jsonb)")
        .execute(&admin).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../../../../../migrations/20261026000100_platform_postgres_jobs.down.sql"
    ))
    .execute(&admin)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../../../migrations/20261026000100_platform_postgres_jobs.sql"
    ))
    .execute(&admin)
    .await
    .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM platform.outbox WHERE id = 456")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let scheduler = Scheduler::new(transactions(db.app_pool()), "after-rollback", vec![]);
    assert!(scheduler.acquire_or_renew().await.unwrap());
}

#[tokio::test]
async fn crashed_scheduler_leaves_interrupted_job_due_for_replacement() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let started = Arc::new(AtomicBool::new(false));
    let signal = started.clone();
    let job = Arc::new(FnJob::new(
        "interrupted",
        Duration::from_secs(3600),
        move || {
            let signal = signal.clone();
            Box::pin(async move {
                signal.store(true, Ordering::SeqCst);
                std::future::pending().await
            })
        },
    ));
    let scheduler = Scheduler::new(transactions(pool.clone()), "crashed", vec![job]);
    let (_shutdown, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        scheduler.run_until_shutdown(rx).await;
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    sqlx::query(
        "UPDATE platform.worker_leases SET expires_at = clock_timestamp() - interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let runs = Arc::new(AtomicUsize::new(0));
    let count = runs.clone();
    let job = Arc::new(FnJob::new(
        "interrupted",
        Duration::from_secs(3600),
        move || {
            let count = count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        },
    ));
    let scheduler = Scheduler::new(transactions(pool.clone()), "replacement", vec![job]);
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        scheduler.run_until_shutdown(rx).await;
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let finished: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM platform.scheduled_jobs WHERE name = 'interrupted' AND last_finished_at IS NOT NULL)")
                .fetch_one(&pool).await.unwrap();
            if finished { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    shutdown.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

fn transactions(pool: sqlx::PgPool) -> sanvi_persistence::TxManager {
    sanvi_persistence::TxManager::new(pool, sanvi_testsupport::test_ids())
}
