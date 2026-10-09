//! Dependency-matrix injection tests: Redis (TASK-023, FR-1112/FR-1113).
//!
//! Injected with a RESP-speaking fault server: tests connect while it is
//! healthy (a real client cannot even establish its connection manager
//! against non-RESP bytes) and flip it to erroring or blackhole — exactly
//! how a production Redis fails under a live process; the 500 ms response
//! timeout bounds the hung case. The matrix's fail-open posture is asserted
//! on the rate limiter, the always-miss degradation on the SSR cache, and
//! outbox durability against a failing stream backend.

mod common;

use common::{RespMode, RespServer};
use redis::aio::ConnectionManager;
use sanvi_http::rate_limit::{LimitError, RateLimiter};

fn limiter_config() -> sanvi_http::RateLimitConfig {
    sanvi_http::RateLimitConfig {
        enabled: true,
        burst: 60,
        per_second: 10,
        fail_open: true,
        exempt_paths: vec![],
    }
}

async fn connection(url: &str) -> ConnectionManager {
    let client = redis::Client::open(url).expect("redis url parses");
    let config = redis::aio::ConnectionManagerConfig::new()
        .set_connection_timeout(Some(std::time::Duration::from_secs(2)))
        .set_response_timeout(Some(std::time::Duration::from_millis(500)));
    ConnectionManager::new_with_config(client, config)
        .await
        .expect("connect to the fault listener")
}

/// Redis · down and slow (same fail-open posture) — the rate limiter
/// reports `BackendUnavailable` instead of lying about budget, and the
/// middleware's configured fail-open posture lets the request through.
#[tokio::test]
async fn redis_down_fails_open_on_the_rate_limiter_and_flags_degraded() {
    // Connected while healthy, then the backend starts failing: first
    // erroring (every command gets `-ERR`), then hung (the response
    // timeout fires).
    let server = RespServer::start();
    let conn = connection(&server.redis_url()).await;
    let degradation = std::sync::Arc::new(sanvi_resilience::DegradationRegistry::default());
    let limiter =
        RateLimiter::new(&limiter_config(), Some(conn)).with_degradation(degradation.clone());

    server.set_mode(RespMode::Error);
    let outcome = limiter.try_acquire("matrix-key").await;
    assert!(
        matches!(outcome, Err(LimitError::BackendUnavailable)),
        "an erroring Redis must report BackendUnavailable, got {outcome:?}"
    );

    // Fail-open is the middleware's policy for BackendUnavailable
    // (`fail_open = true` in the shipping posture): a request proceeds.
    // The documented wire consequence is the degraded flag: the registry
    // carries `redis` while the backend is unavailable.
    assert_eq!(degradation.header_value().as_deref(), Some("redis"));
    let state = std::sync::Arc::new(
        sanvi_http::LoadShedState::new(
            sanvi_http::LoadSheddingConfig {
                enabled: true,
                in_flight_ceiling: 4,
                fairness_knee: 4,
                ..Default::default()
            },
            None,
        )
        .with_degradation(degradation.clone()),
    );
    let _first = state.admit("/probe", None);
    let _second = state.admit("/probe", None);
    assert!(
        matches!(
            state.admit("/probe", None),
            sanvi_http::AdmitOutcome::Shed(_)
        ),
        "Redis outage must halve admission capacity"
    );
    assert!(matches!(
        state.admit("/api/v1/payments/webhooks/stripe", None),
        sanvi_http::AdmitOutcome::Admitted(_)
    ));
    // Recover the same connection and limiter, rather than setting the flag in the test.
    server.set_mode(RespMode::Ok);
    let recovery_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if limiter.try_acquire("matrix-recovered").await.is_ok() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < recovery_deadline,
            "managed Redis connection did not recover"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(degradation.header_value(), None);
    assert!(matches!(
        state.admit("/probe", None),
        sanvi_http::AdmitOutcome::Admitted(_)
    ));
    // A swallowed RESP reply cannot be repaired on the same socket: a
    // blackhole proves the deadline separately from error/recovery above.
    server.set_mode(RespMode::Blackhole);
    let started = std::time::Instant::now();
    assert!(matches!(
        limiter.try_acquire("matrix-key").await,
        Err(LimitError::BackendUnavailable)
    ));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(300),
        "the limiter's 100ms budget bounds a hung Redis"
    );
    assert_eq!(degradation.header_value().as_deref(), Some("redis"));
    // The independent probe also observes the same outage.
    sanvi_http::rate_limit::probe_redis(None, &degradation).await;
    assert!(degradation.is_active(sanvi_resilience::DegradationReason::Redis));
}

/// Redis · healthy control — the same limiter against the shared test
/// Redis admits within burst, proving `BackendUnavailable` above came
/// from the injected failure, not the limiter.
#[tokio::test]
async fn redis_healthy_limiter_admits_normally() {
    let url = sanvi_testsupport::redis_url().await;
    let conn = connection(&url).await;
    let limiter = RateLimiter::new(&limiter_config(), Some(conn));
    let key = format!("matrix-{}", std::process::id());
    for _ in 0..5 {
        limiter
            .try_acquire(&key)
            .await
            .expect("a healthy Redis admits within burst");
    }
}

/// Redis · erroring — the SSR cache degrades to an always-miss: reads
/// answer `None`, writes are swallowed, nothing panics, nothing serves a
/// stale tenant's page.
#[tokio::test]
async fn redis_erroring_degrades_to_cache_misses() {
    // Connected while healthy, then every command errors.
    let server = RespServer::start();
    let conn = connection(&server.redis_url()).await;
    server.set_mode(RespMode::Error);
    let cache = sanvi_cache::SsrCache::new(Some(conn), None);

    let key = sanvi_cache::SsrCacheKey::new(
        sanvi_kernel::TenantId::try_from(1_000_001_i64).unwrap(),
        "en",
        1,
        sanvi_cache::DeviceClass::Desktop,
        "/",
    )
    .expect("valid key parts");
    let entry = sanvi_cache::SsrEntry {
        status: 200,
        headers: vec![("content-type".to_string(), "text/html".to_string())],
        body: b"<html></html>".to_vec(),
    };
    cache
        .put(&key, &entry, std::time::Duration::from_secs(60))
        .await;
    assert!(
        cache.get(&key).await.is_none(),
        "an erroring Redis must degrade to an always-miss"
    );
}

/// Redis · down — the outbox table is the durability boundary: with the
/// stream backend failing, a dispatch attempt records the failure, the
/// row stays unpublished (nothing lost, nothing dead-lettered before its
/// attempt budget), and the next poll retries.
#[tokio::test]
async fn redis_down_pauses_streams_without_losing_the_outbox() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();

    sqlx::query(
        "INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload, metadata, attempts)
         VALUES (987654321, 'tenants', 42, 'tenants.tenant_created', '{}', '{}', 0)",
    )
    .execute(&pool)
    .await
    .expect("seed outbox row");

    // Connected while healthy, then every command errors: an erroring
    // stream backend.
    let server = RespServer::start();
    let conn = connection(&server.redis_url()).await;
    server.set_mode(RespMode::Error);
    let streams = sanvi_messaging::StreamsClient::new(conn);
    let dispatcher = sanvi_messaging::OutboxDispatcher::new(
        pool.clone(),
        streams,
        sanvi_messaging::StreamName("sanvi.events".to_string()),
        100,
        5,
    );

    let published = dispatcher
        .run_once()
        .await
        .expect("a failed publish is recorded, not propagated: the dispatcher keeps going");
    assert_eq!(published, 0, "nothing reached the stream");

    // The row survived: unpublished, attempt counted, not dead-lettered.
    let row: (Option<chrono::DateTime<chrono::Utc>>, i32) =
        sqlx::query_as("SELECT published_at, attempts FROM platform.outbox WHERE id = 987654321")
            .fetch_one(&pool)
            .await
            .expect("the outbox row must survive a failed publish");
    assert!(
        row.0.is_none(),
        "an unpublished row keeps a NULL published_at"
    );
    assert_eq!(row.1, 1, "exactly one failed attempt is recorded");
    // The app role cannot read the DLQ table (INSERT only, by design) —
    // the operator-level check runs on the admin pool.
    let dead: (i64,) = sqlx::query_as("SELECT count(*) FROM platform.outbox_dlq")
        .fetch_one(&db.admin_pool().await)
        .await
        .unwrap();
    assert_eq!(dead.0, 0, "one failed publish must not dead-letter");
}
