//! Job retry budget (TASK-023 step 7): a permanently failing scheduled job
//! stops running at its budget instead of burning capacity forever.
//!
//! The scheduler applies one budget per job name: every consecutive failure
//! doubles a skip window, and once `max_consecutive_failures` is reached
//! the job suspends — skipped with a warning and a metric — until the
//! operator replays it (the DLQ/CLI surface) or the skip window lapses and
//! one probe run is allowed to re-learn the outcome.

use std::time::{Duration, Instant};

/// The policy constants, one per job in the matrix/runbooks; defaults are
/// the documented posture (5 failures, 30 s base, 1 h ceiling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobRetryPolicy {
    /// Consecutive failures before the job suspends.
    pub max_consecutive_failures: u32,
    /// First skip window after the first failure.
    pub base_backoff_secs: u64,
    /// Skip-window ceiling.
    pub max_backoff_secs: u64,
}

impl Default for JobRetryPolicy {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 5,
            base_backoff_secs: 30,
            max_backoff_secs: 3600,
        }
    }
}

/// What the scheduler should do with this job on this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// Run the job.
    Run,
    /// Skip this tick; retry no earlier than the carried instant.
    SkipUntil(Instant),
}

#[derive(Debug)]
struct State {
    consecutive_failures: u32,
    suspended_since: Option<Instant>,
}

/// One budget per job name. Not `Clone` on purpose: the budget is state.
pub struct JobRetryBudget {
    policy: JobRetryPolicy,
    inner: std::sync::Mutex<State>,
}

impl JobRetryBudget {
    pub fn new(policy: JobRetryPolicy) -> Self {
        Self {
            policy,
            inner: std::sync::Mutex::new(State {
                consecutive_failures: 0,
                suspended_since: None,
            }),
        }
    }

    /// May the job run on this tick?
    pub fn admit(&self) -> RetryOutcome {
        let inner = self.inner.lock().expect("retry budget lock");
        match inner.suspended_since {
            None => RetryOutcome::Run,
            Some(since) => {
                let window = self.skip_window(inner.consecutive_failures);
                if since.elapsed() >= window {
                    // One probe run after the window lapses: the budget
                    // re-learns the outcome from `record_*` below.
                    RetryOutcome::Run
                } else {
                    RetryOutcome::SkipUntil(since + window)
                }
            }
        }
    }

    pub fn record_success(&self) {
        let mut inner = self.inner.lock().expect("retry budget lock");
        inner.consecutive_failures = 0;
        inner.suspended_since = None;
    }

    pub fn record_failure(&self) {
        let mut inner = self.inner.lock().expect("retry budget lock");
        inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
        if inner.consecutive_failures >= self.policy.max_consecutive_failures {
            if inner.suspended_since.is_none() {
                tracing::warn!(
                    failures = inner.consecutive_failures,
                    "job suspended at its retry budget"
                );
            }
            inner.suspended_since.get_or_insert(Instant::now());
        }
    }

    /// Backoff doubling capped at the policy ceiling, counted from the
    /// first failure.
    fn skip_window(&self, failures: u32) -> Duration {
        let exponent = failures.saturating_sub(1).min(16);
        let secs = self
            .policy
            .base_backoff_secs
            .saturating_mul(1u64 << exponent)
            .min(self.policy.max_backoff_secs);
        Duration::from_secs(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> JobRetryBudget {
        JobRetryBudget::new(JobRetryPolicy {
            max_consecutive_failures: 3,
            base_backoff_secs: 30,
            max_backoff_secs: 3600,
        })
    }

    #[test]
    fn runs_until_the_budget_then_suspends() {
        let budget = budget();
        assert_eq!(budget.admit(), RetryOutcome::Run);
        budget.record_failure();
        assert_eq!(budget.admit(), RetryOutcome::Run, "under budget: still runs");
        budget.record_failure();
        assert_eq!(budget.admit(), RetryOutcome::Run);
        budget.record_failure();
        match budget.admit() {
            RetryOutcome::SkipUntil(until) => {
                assert!(until > std::time::Instant::now());
            }
            RetryOutcome::Run => panic!("a job at its budget must suspend"),
        }
    }

    #[test]
    fn success_resets_the_budget() {
        let budget = budget();
        for _ in 0..3 {
            budget.record_failure();
        }
        budget.record_success();
        assert_eq!(budget.admit(), RetryOutcome::Run);
        for _ in 0..2 {
            budget.record_failure();
        }
        assert_eq!(budget.admit(), RetryOutcome::Run);
    }

    #[test]
    fn skip_window_doubles_and_caps() {
        let budget = budget();
        // failures=1 → base, failures=4 → 30*8 = 240, failures=20 → cap.
        assert_eq!(budget.skip_window(1), Duration::from_secs(30));
        assert_eq!(budget.skip_window(4), Duration::from_secs(240));
        assert_eq!(budget.skip_window(20), Duration::from_secs(3600));
    }
}
