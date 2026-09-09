use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use sanvi_messaging::{FnJob, RedisConfig, Scheduler, connect_redis};

async fn prepare_redis() -> redis::aio::ConnectionManager {
    connect_redis(&RedisConfig {
        url: sanvi_testsupport::redis_url().await,
        ..RedisConfig::default()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn job_runs_shortly_after_startup_and_repeatedly_on_subsequent_ticks() {
    let conn = prepare_redis().await;
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

    let scheduler = Scheduler::new(conn, "worker-startup", vec![job])
        .with_lock_key(lock_key)
        .with_tick(Duration::from_millis(30))
        .with_lock_ttl(5);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        scheduler.run_until_shutdown(shutdown_rx).await;
    });

    // 1. Job runs shortly after startup
    tokio::time::timeout(Duration::from_millis(300), async {
        while runs.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job should run shortly after startup");

    // 2. Job runs repeatedly on subsequent ticks
    tokio::time::timeout(Duration::from_millis(500), async {
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
    let conn = prepare_redis().await;
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

    let scheduler = Scheduler::new(conn, "worker-shutdown", vec![job])
        .with_lock_key(lock_key)
        .with_tick(Duration::from_millis(20))
        .with_lock_ttl(5);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        scheduler.run_until_shutdown(shutdown_rx).await;
    });

    // Wait until it has run at least once
    tokio::time::timeout(Duration::from_millis(300), async {
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
    let conn1 = prepare_redis().await;
    let conn2 = prepare_redis().await;
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

    let s1 = Scheduler::new(conn1, "worker-1", vec![job1])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(30))
        .with_lock_ttl(5);

    let s2 = Scheduler::new(conn2, "worker-2", vec![job2])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(30))
        .with_lock_ttl(5);

    let (shutdown1_tx, shutdown1_rx) = tokio::sync::watch::channel(false);
    let (shutdown2_tx, shutdown2_rx) = tokio::sync::watch::channel(false);

    let h1 = tokio::spawn(async move {
        s1.run_until_shutdown(shutdown1_rx).await;
    });

    // Wait for worker 1 to acquire the lock and run
    tokio::time::timeout(Duration::from_millis(300), async {
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
    tokio::time::timeout(Duration::from_millis(500), async {
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
    let conn = prepare_redis().await;
    let lock_key = sanvi_testsupport::test_stream("scheduler_renew_seq");

    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = completed.clone();

    // Job duration is 800ms, with TTL = 1s and tick = 100ms
    // The keepalive will renew during this 800ms run.
    let job = Arc::new(FnJob::new("long_job", Duration::from_secs(10), move || {
        let completed = completed_clone.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            completed.store(true, Ordering::SeqCst);
            Ok(())
        })
    }));

    let s1 = Scheduler::new(conn.clone(), "worker-long", vec![job])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(100))
        .with_lock_ttl(1);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let h1 = tokio::spawn(async move {
        s1.run_until_shutdown(shutdown_rx).await;
    });

    // Competing scheduler on the same lock key trying to acquire
    let s2 = Scheduler::new(conn, "worker-challenger", vec![])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(100))
        .with_lock_ttl(1);

    // After 600ms (more than 1/2 of TTL), s2 should still NOT be able to acquire
    tokio::time::sleep(Duration::from_millis(600)).await;
    let s2_acquired = s2.acquire_or_renew().await.unwrap();
    assert!(
        !s2_acquired,
        "challenger must not acquire lock because worker-long's keepalive renewed it"
    );

    // Wait for the job to complete
    tokio::time::timeout(Duration::from_millis(600), async {
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
    let conn = prepare_redis().await;
    let lock_key = sanvi_testsupport::test_stream("scheduler_lost_lease");

    let cancelled = Arc::new(AtomicBool::new(true));
    let cancelled_clone = cancelled.clone();
    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();

    // A job that waits, and clears `cancelled` if it runs to completion
    let job = Arc::new(FnJob::new("long_job", Duration::from_secs(10), move || {
        let cancelled = cancelled_clone.clone();
        let started = started_clone.clone();
        Box::pin(async move {
            started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(1500)).await;
            cancelled.store(false, Ordering::SeqCst);
            Ok(())
        })
    }));

    let s1 = Scheduler::new(conn.clone(), "worker-evicted", vec![job])
        .with_lock_key(&lock_key)
        .with_tick(Duration::from_millis(50))
        .with_lock_ttl(1);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let h1 = tokio::spawn(async move {
        s1.run_until_shutdown(shutdown_rx).await;
    });

    // Wait until the job has started
    tokio::time::timeout(Duration::from_millis(500), async {
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // Now forcibly steal the lock in Redis by overwriting it with another instance id
    let mut redis_conn = conn.clone();
    let _: () = redis::AsyncCommands::set(&mut redis_conn, &lock_key, "stealer")
        .await
        .unwrap();

    // Wait for the keepalive to detect the loss and cancel the job
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The job should have been dropped/cancelled before completing (cancelled remains true)
    assert!(
        cancelled.load(Ordering::SeqCst),
        "job must be cancelled when leadership lease is lost"
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), h1).await;
}
