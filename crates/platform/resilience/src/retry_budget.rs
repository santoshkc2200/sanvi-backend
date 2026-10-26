//! Job retry budget (TASK-023 step 7): a permanently failing scheduled job
//! stops running at its budget instead of burning capacity forever.
//!
//! The scheduler persists each job's consecutive failures. Once
//! `max_consecutive_failures` is reached, the next run waits a skip window
//! that doubles with every further failure, then one probe run re-learns
//! the outcome. A success resets the count.

use std::time::Duration;

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

impl JobRetryPolicy {
    /// Backoff doubling capped at the policy ceiling, counted from the
    /// first failure.
    pub fn skip_window(&self, failures: u32) -> Duration {
        let exponent = failures.saturating_sub(1).min(16);
        let secs = self
            .base_backoff_secs
            .saturating_mul(1u64 << exponent)
            .min(self.max_backoff_secs);
        Duration::from_secs(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_window_doubles_and_caps() {
        let policy = JobRetryPolicy {
            max_consecutive_failures: 3,
            base_backoff_secs: 30,
            max_backoff_secs: 3600,
        };
        // failures=1 → base, failures=4 → 30*8 = 240, failures=20 → cap.
        assert_eq!(policy.skip_window(1), Duration::from_secs(30));
        assert_eq!(policy.skip_window(4), Duration::from_secs(240));
        assert_eq!(policy.skip_window(20), Duration::from_secs(3600));
    }
}
