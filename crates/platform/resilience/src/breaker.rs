//! Circuit breaker for outbound dependency calls (TASK-023, NFR-1105).
//!
//! Classic three-state breaker: *closed* (normal, failures counted), *open*
//! (calls fail fast until the cooldown elapses), *half-open* (a bounded
//! number of probes decide recovery). Every dependency call the dependency
//! matrix names wraps its I/O in [`CircuitBreaker::call`]; the per-row
//! injection tests in `tests/resilience` assert the matrix-documented
//! behaviour through it.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use prometheus::{IntCounterVec, IntGaugeVec, Opts, Registry};
/// Breaker tuning. Defaults are the dependency matrix's stated thresholds;
/// a row that needs different numbers declares them at construction so the
/// matrix and the code cannot drift apart silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive failures that open the breaker.
    pub failure_threshold: u32,
    /// How long an open breaker refuses calls before probing again.
    pub open_secs: u64,
    /// Concurrent probe calls allowed in the half-open state.
    pub half_open_max_probes: u32,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_secs: 30,
            half_open_max_probes: 1,
        }
    }
}

impl BreakerConfig {
    /// Thresholds for a dependency whose matrix row states them. Keeps the
    /// numbers in one place per call site instead of scattering literals.
    pub const fn new(failure_threshold: u32, open_secs: u64) -> Self {
        Self {
            failure_threshold,
            open_secs,
            half_open_max_probes: 1,
        }
    }
}

/// Why a call did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("circuit breaker is open")]
pub struct BreakerOpen;

/// The three breaker states, exposed for metrics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Closed => 0,
            Self::Open => 1,
            Self::HalfOpen => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

#[derive(Debug)]
struct Inner {
    state: BreakerState,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    half_open_probes: u32,
}

/// A named, metrics-carrying circuit breaker. Cheap to clone behind an
/// `Arc`; a breaker per adapter instance (one per dependency), never per
/// call.
pub struct CircuitBreaker {
    name: &'static str,
    config: BreakerConfig,
    inner: Mutex<Inner>,
    state_gauges: IntGaugeVec,
    transitions: IntCounterVec,
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("name", &self.name)
            .field("config", &self.config)
            .finish()
    }
}

/// A permission to run one call. Drop it without calling [`Permit::report`]
/// to count the call as a failure — the panic path lands there too, which
/// is the conservative reading of "the dependency misbehaved".
pub struct Permit<'a> {
    breaker: &'a CircuitBreaker,
    counted: bool,
}

impl Permit<'_> {
    /// Record the call's outcome and release the permit.
    pub fn report(mut self, ok: bool) {
        self.counted = true;
        self.breaker.report(ok);
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if !self.counted {
            self.breaker.report(false);
        }
    }
}

impl CircuitBreaker {
    /// `registry` may be `None` in tests; the breaker then runs unrecorded.
    pub fn new(name: &'static str, config: BreakerConfig, registry: Option<&Registry>) -> Self {
        let state_gauges = IntGaugeVec::new(
            Opts::new(
                "resilience_breaker_state",
                "Circuit breaker state: 0 closed, 1 open, 2 half-open",
            ),
            &["name"],
        )
        .expect("breaker gauge definition is valid");
        let transitions = IntCounterVec::new(
            Opts::new(
                "resilience_breaker_transitions_total",
                "Circuit breaker state transitions",
            ),
            &["name", "from", "to"],
        )
        .expect("breaker counter definition is valid");
        if let Some(registry) = registry {
            // One registry sees many breakers; a duplicate registration is
            // a construction bug, not a runtime condition, so the gauge
            // family registers once and the per-name children are distinct.
            registry.register(Box::new(state_gauges.clone())).ok();
            registry.register(Box::new(transitions.clone())).ok();
        }
        state_gauges
            .with_label_values(&[name])
            .set(i64::from(BreakerState::Closed.as_u8()));
        Self {
            name,
            config,
            inner: Mutex::new(Inner {
                state: BreakerState::Closed,
                consecutive_failures: 0,
                opened_at: None,
                half_open_probes: 0,
            }),
            state_gauges,
            transitions,
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Current state (for tests and readiness surfaces).
    pub fn state(&self) -> BreakerState {
        self.inner.lock().expect("breaker lock").state
    }

    /// Run `f`'s future under the breaker. When the breaker is open (or the
    /// half-open probe budget is exhausted) the future is not constructed
    /// and [`BreakerOpen`] is returned immediately.
    pub async fn call<T, E, F, Fut>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: From<BreakerOpen>,
    {
        let permit = match self.acquire() {
            Ok(permit) => permit,
            Err(open) => return Err(open.into()),
        };
        let result = f().await;
        permit.report(result.is_ok());
        result
    }

    /// Ask for permission to run one call. Sync and lock-free enough: the
    /// critical section never awaits.
    pub fn acquire(&self) -> Result<Permit<'_>, BreakerOpen> {
        let mut inner = self.inner.lock().expect("breaker lock");
        match inner.state {
            BreakerState::Closed => Ok(Permit {
                breaker: self,
                counted: false,
            }),
            BreakerState::Open => {
                let elapsed = inner
                    .opened_at
                    .map(|at| at.elapsed())
                    .unwrap_or(Duration::ZERO);
                if elapsed >= Duration::from_secs(self.config.open_secs) {
                    self.transition(&mut inner, BreakerState::HalfOpen);
                    inner.half_open_probes = 1;
                    Ok(Permit {
                        breaker: self,
                        counted: false,
                    })
                } else {
                    Err(BreakerOpen)
                }
            }
            BreakerState::HalfOpen => {
                if inner.half_open_probes < self.config.half_open_max_probes {
                    inner.half_open_probes += 1;
                    Ok(Permit {
                        breaker: self,
                        counted: false,
                    })
                } else {
                    Err(BreakerOpen)
                }
            }
        }
    }

    /// Record one call outcome. Public so a permit's Drop path and manual
    /// call sites share exactly one transition rule.
    pub fn report(&self, ok: bool) {
        let mut inner = self.inner.lock().expect("breaker lock");
        match inner.state {
            BreakerState::Closed => {
                if ok {
                    inner.consecutive_failures = 0;
                } else {
                    inner.consecutive_failures += 1;
                    if inner.consecutive_failures >= self.config.failure_threshold {
                        self.transition(&mut inner, BreakerState::Open);
                        inner.opened_at = Some(Instant::now());
                    }
                }
            }
            BreakerState::HalfOpen => {
                inner.half_open_probes = inner.half_open_probes.saturating_sub(1);
                if ok {
                    self.transition(&mut inner, BreakerState::Closed);
                    inner.consecutive_failures = 0;
                    inner.opened_at = None;
                } else {
                    self.transition(&mut inner, BreakerState::Open);
                    inner.opened_at = Some(Instant::now());
                }
            }
            BreakerState::Open => {
                // A straggler from the closed window reported late; nothing
                // to do but ignore it.
            }
        }
    }

    fn transition(&self, inner: &mut Inner, to: BreakerState) {
        let from = inner.state;
        if from == to {
            return;
        }
        inner.state = to;
        self.state_gauges
            .with_label_values(&[self.name])
            .set(i64::from(to.as_u8()));
        self.transitions
            .with_label_values(&[self.name, from.as_str(), to.as_str()])
            .inc();
        tracing::warn!(
            breaker = self.name,
            from = from.as_str(),
            to = to.as_str(),
            "circuit breaker transition"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error type a test call site uses; satisfies the same
    /// `From<BreakerOpen>` bound every real adapter error will.
    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error("boom")]
        Fail,
        #[error("circuit breaker is open")]
        Open(#[from] BreakerOpen),
    }

    fn breaker() -> CircuitBreaker {
        CircuitBreaker::new("test", BreakerConfig::new(3, 3_600), None)
    }

    /// `open_secs = 0`: an open breaker probes again on the next acquire.
    fn probing_breaker() -> CircuitBreaker {
        CircuitBreaker::new("test", BreakerConfig::new(3, 0), None)
    }

    async fn failing(breaker: &CircuitBreaker) -> Result<(), TestError> {
        breaker.call(|| async { Err(TestError::Fail) }).await
    }

    #[tokio::test]
    async fn opens_after_the_failure_threshold_and_fails_fast() {
        let breaker = breaker();
        for _ in 0..3 {
            assert!(failing(&breaker).await.is_err());
        }
        assert_eq!(breaker.state(), BreakerState::Open);
        // Open: the call never runs.
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_clone = ran.clone();
        let result: Result<(), TestError> = breaker
            .call(|| async {
                ran_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(matches!(result, Err(TestError::Open(_))));
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_count() {
        let breaker = breaker();
        for _ in 0..2 {
            assert!(failing(&breaker).await.is_err());
        }
        let _: Result<u8, TestError> = breaker.call(|| async { Ok(1) }).await;
        for _ in 0..2 {
            assert!(failing(&breaker).await.is_err());
        }
        assert_eq!(breaker.state(), BreakerState::Closed, "2 < 3: still closed");
    }

    #[tokio::test]
    async fn half_open_probe_success_closes_and_failure_reopens() {
        let breaker = probing_breaker();
        for _ in 0..3 {
            assert!(failing(&breaker).await.is_err());
        }
        assert_eq!(breaker.state(), BreakerState::Open);
        // open_secs = 0: the next acquire transitions to half-open and runs
        // the single allowed probe.
        let ok: Result<u8, TestError> = breaker.call(|| async { Ok(1) }).await;
        assert!(ok.is_ok());
        assert_eq!(breaker.state(), BreakerState::Closed);

        for _ in 0..3 {
            assert!(failing(&breaker).await.is_err());
        }
        // Failing probe reopens.
        assert!(failing(&breaker).await.is_err());
        assert_eq!(breaker.state(), BreakerState::Open);
    }

    #[tokio::test]
    async fn dropped_permit_counts_as_a_failure() {
        let breaker = breaker();
        for _ in 0..2 {
            assert!(failing(&breaker).await.is_err());
        }
        {
            let _permit = breaker.acquire().unwrap();
            // Dropped without report: a panic or an abandoned call.
        }
        assert_eq!(breaker.state(), BreakerState::Open);
    }

    #[test]
    fn acquire_on_a_closed_breaker_reports_through_the_permit() {
        let breaker = breaker();
        let permit = breaker.acquire().expect("closed breaker admits");
        permit.report(false);
        let permit = breaker.acquire().expect("one failure stays closed");
        permit.report(true);
        assert_eq!(breaker.state(), BreakerState::Closed);
    }
}
