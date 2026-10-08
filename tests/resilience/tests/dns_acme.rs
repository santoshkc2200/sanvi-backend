//! Dependency-matrix injection tests: DNS/ACME (TASK-023, FR-1112).
//!
//! The DoH verifier is pointed at real fault resolvers: the N-majority
//! tolerates failing and slow members, and a fully dead resolver set
//! fails the round (the domain stays pending; the poll retries).

mod common;

use common::{FaultMode, FaultServer};
use sanvi_domains::adapters::DohDnsVerifier;
use sanvi_domains::domain::ports::DnsVerifierPort;
use std::time::Duration;

const EXPECTED: &str = "sanvi-verify=matrix";

fn doh_body(value: &str) -> String {
    format!(
        r#"{{"Answer":[{{"name":"_sanvi-challenge.example.com","type":16,"data":"\"{value}\""}}]}}"#
    )
}

/// DNS · slow — a resolver that answers past its deadline drops out of the
/// majority; the remaining two still confirm.
#[tokio::test]
async fn dns_slow_resolver_drops_out_of_the_majority() {
    let slow = FaultServer::start(FaultMode::Delay {
        delay_ms: 5_000,
        status: 200,
    });
    let fast_a = FaultServer::start(FaultMode::Body(doh_body(EXPECTED)));
    let fast_b = FaultServer::start(FaultMode::Body(doh_body(EXPECTED)));

    // The verifier deadline (1s) is the slow resolver's verdict: too slow
    // is failing.
    let verifier = DohDnsVerifier::new(
        vec![
            slow.base_url().to_string(),
            fast_a.base_url().to_string(),
            fast_b.base_url().to_string(),
        ],
        2,
        1,
    );
    let hostname = sanvi_domains::domain::Hostname::parse("example.com").unwrap();
    let started = std::time::Instant::now();
    let result = verifier
        .verify_txt(&hostname, EXPECTED)
        .await
        .expect("two healthy resolvers still answer");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the per-verifier deadline must bound the round, took {:?}",
        started.elapsed()
    );
    assert!(
        result.confirmed,
        "2 of 3 (one slow) must confirm: {:?}",
        result.detail
    );
    assert_eq!(result.resolvers_agreed, 2);
}

/// DNS · erroring — 2 of 3 resolvers 500ing is tolerated by the majority;
/// the check still confirms from the healthy one plus... nothing: with
/// only one agreeing resolver and min_agreement 2 the round is *not*
/// confirmed, and with all failing it is an error.
#[tokio::test]
async fn dns_erroring_resolvers_tolerated_up_to_the_majority() {
    // Two healthy, one erroring: majority reached.
    let erroring = FaultServer::start(FaultMode::Status(500));
    let healthy_a = FaultServer::start(FaultMode::Body(doh_body(EXPECTED)));
    let healthy_b = FaultServer::start(FaultMode::Body(doh_body(EXPECTED)));
    let verifier = DohDnsVerifier::new(
        vec![
            erroring.base_url().to_string(),
            healthy_a.base_url().to_string(),
            healthy_b.base_url().to_string(),
        ],
        2,
        2,
    );
    let hostname = sanvi_domains::domain::Hostname::parse("example.com").unwrap();
    let result = verifier
        .verify_txt(&hostname, EXPECTED)
        .await
        .expect("a round with any answering resolver returns a result");
    assert!(result.confirmed, "2 of 3 (one erroring) must confirm");

    // Below the majority (only one healthy), the round is not confirmed —
    // the domain stays pending rather than confirming on thin evidence.
    let erroring_b = FaultServer::start(FaultMode::Status(500));
    let thin = DohDnsVerifier::new(
        vec![
            erroring.base_url().to_string(),
            erroring_b.base_url().to_string(),
            healthy_a.base_url().to_string(),
        ],
        2,
        2,
    );
    let result = thin
        .verify_txt(&hostname, EXPECTED)
        .await
        .expect("a round with any answering resolver returns a result");
    assert!(
        !result.confirmed,
        "1 of 3 must not confirm: {:?}",
        result.detail
    );
}

/// DNS · down — every resolver unreachable: the round fails with
/// `VerificationUnavailable`; the domain stays pending for the next poll.
#[tokio::test]
async fn dns_down_fails_the_round_and_stays_pending() {
    let (dead_a, _) = common::closed_endpoint();
    let (dead_b, _) = common::closed_endpoint();
    let (dead_c, _) = common::closed_endpoint();
    let verifier = DohDnsVerifier::new(vec![dead_a, dead_b, dead_c], 2, 1);
    let hostname = sanvi_domains::domain::Hostname::parse("example.com").unwrap();
    let err = verifier
        .verify_txt(&hostname, EXPECTED)
        .await
        .expect_err("no resolver answering fails the round");
    assert!(
        matches!(
            err,
            sanvi_domains::domain::DomainsError::VerificationUnavailable(_)
        ),
        "expected VerificationUnavailable, got: {err}"
    );
}
