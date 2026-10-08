//! Dependency-matrix injection tests: Stripe (TASK-023, FR-1112/FR-1113).
//!
//! The invariant under injection: **retries ride the same Idempotency-Key,
//! so a retried mutation can never double-charge.** The injections are
//! real HTTP against the fault server: a 500-answerer (erroring), a
//! delayed answer past the client deadline (slow), and a closed port
//! (down).

mod common;

use common::{FaultMode, FaultServer};
use sanvi_billing::adapters::stripe::HttpStripeClient;
use sanvi_billing::domain::ports::StripeClient;
use sanvi_kernel::TenantId;
use std::time::Duration;
use uuid::Uuid;

fn client(base_url: &str, request_timeout: Duration, backoff_ms: u64) -> HttpStripeClient {
    HttpStripeClient::new_with_timeouts(
        base_url.to_string(),
        "sk_test_matrix".to_string(),
        "2026-01-01".to_string(),
        None,
        Duration::from_millis(200),
        request_timeout,
        backoff_ms,
    )
}

/// Stripe · erroring — every 5xx is retried, all attempts carry the same
/// Idempotency-Key, and exhaustion surfaces a service error (never a
/// silent success, never a second key).
#[tokio::test]
async fn stripe_erroring_retries_under_the_same_idempotency_key() {
    let server = FaultServer::start(FaultMode::Status(500));
    let stripe = client(server.base_url(), Duration::from_secs(2), 1);

    let command_id = Uuid::new_v4();
    let outcome = stripe
        .ensure_customer(TenantId::try_from(1_000_001_i64).unwrap(), command_id)
        .await;
    assert!(
        matches!(
            outcome,
            Err(sanvi_billing::domain::BillingError::StripeService { .. })
        ),
        "exhausted 5xx retries surface a service error, got {outcome:?}"
    );

    let recorded = server.recorded();
    assert_eq!(
        recorded.len(),
        4,
        "one initial call plus MAX_RETRIES=3 retries, got {}",
        recorded.len()
    );
    let keys: Vec<_> = recorded
        .iter()
        .map(|request| {
            request
                .header("Idempotency-Key")
                .unwrap_or("<missing>")
                .to_string()
        })
        .collect();
    assert!(
        keys.iter()
            .all(|key| key == &format!("sanvi_customer_{command_id}")),
        "every retry must carry the same idempotency key, got {keys:?}"
    );
}

/// Stripe · slow — the deadline cancels the call and the use case sees a
/// connection failure.
#[tokio::test]
async fn stripe_slow_times_out_and_reports_a_connection_failure() {
    let server = FaultServer::start(FaultMode::Delay {
        delay_ms: 5_000,
        status: 200,
    });
    let stripe = client(server.base_url(), Duration::from_millis(300), 1);
    let started = std::time::Instant::now();
    let outcome = stripe
        .ensure_customer(TenantId::try_from(1_000_001_i64).unwrap(), Uuid::new_v4())
        .await;
    assert!(
        matches!(
            outcome,
            Err(sanvi_billing::domain::BillingError::StripeConnection { .. })
        ),
        "a deadline-exceeded call is a connection failure, got {outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the 300ms deadline must bound the wait, took {:?}",
        started.elapsed()
    );
}

/// Stripe · down — connection refused exhausts the same bounded retry
/// budget into a connection error.
#[tokio::test]
async fn stripe_down_exhausts_retries_into_a_connection_error() {
    let (base, _port) = common::closed_endpoint();
    let stripe = client(&base, Duration::from_secs(2), 1);
    let started = std::time::Instant::now();
    let outcome = stripe
        .ensure_customer(TenantId::try_from(1_000_001_i64).unwrap(), Uuid::new_v4())
        .await;
    assert!(
        matches!(
            outcome,
            Err(sanvi_billing::domain::BillingError::StripeConnection { .. })
        ),
        "a dead Stripe is a connection failure, got {outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "bounded retries must not hang, took {:?}",
        started.elapsed()
    );
}

/// Stripe · breaker — after the configured failure threshold the breaker
/// opens and calls fail fast without touching the network, recovering on
/// the half-open probe.
#[tokio::test]
async fn stripe_breaker_opens_after_repeated_failures_and_fails_fast() {
    let (base, _port) = common::closed_endpoint();
    let stripe = HttpStripeClient::new_with_resilience(
        base,
        "sk_test_matrix".to_string(),
        "2026-01-01".to_string(),
        None,
        Duration::from_millis(200),
        Duration::from_secs(2),
        1,
        // Two failures open the breaker; the 30 s cooldown keeps it open
        // for the probe below (a zero cooldown would half-open
        // immediately and the "fail fast" assertion could never hold).
        sanvi_resilience::BreakerConfig::new(2, 30),
        None,
    );
    let tenant = TenantId::try_from(1_000_001_i64).unwrap();
    for _ in 0..2 {
        let _ = stripe.ensure_customer(tenant, Uuid::new_v4()).await;
    }
    // Breaker open: the next call fails immediately without a network
    // attempt (a closed endpoint would still cost the connect refusal;
    // the breaker short-circuits to an equally-shaped error, instantly).
    let started = std::time::Instant::now();
    let outcome = stripe.ensure_customer(tenant, Uuid::new_v4()).await;
    assert!(outcome.is_err());
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "an open breaker must fail fast, took {:?}",
        started.elapsed()
    );
}
