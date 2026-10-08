//! The pool-pressure signal (TASK-023 step 6, FR-1114): the number the
//! load shedder's `pool_wait_micros` threshold reads.
//!
//! The pool sampler feeds this on every probe; the HTTP shedder reads it on
//! admission. A sample older than [`MAX_SAMPLE_AGE`] is ignored — a stale
//! wait must not shed traffic minutes after the pool recovered. Plain
//! atomics, no Prometheus read-back: the gauges stay the observability
//! surface, this is the control-plane one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Samples older than this never shed: without a freshness bound a single
/// slow probe during a deploy would throttle traffic until the next sample
/// lands.
pub const MAX_SAMPLE_AGE: Duration = Duration::from_secs(5);

/// Microseconds of pool acquire wait from the latest probe, with its sample
/// time. One per process, shared by the sampler (writer) and the shedder
/// (reader).
#[derive(Debug, Default)]
pub struct PoolPressure {
    wait_micros: AtomicU64,
    sampled_at_millis: AtomicU64,
}

impl PoolPressure {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a probe result. Called by the pool sampler, not by requests.
    pub fn sample(&self, wait: Duration) {
        self.wait_micros.store(
            wait.as_micros().min(u64::MAX as u128) as u64,
            Ordering::SeqCst,
        );
        self.sampled_at_millis.store(now_millis(), Ordering::SeqCst);
    }

    /// The latest wait, or `None` when no sample exists yet or the latest
    /// one is older than [`MAX_SAMPLE_AGE`].
    pub fn current_wait(&self) -> Option<Duration> {
        let sampled = self.sampled_at_millis.load(Ordering::SeqCst);
        if sampled == 0 || now_millis().saturating_sub(sampled) > MAX_SAMPLE_AGE.as_millis() as u64
        {
            return None;
        }
        Some(Duration::from_micros(
            self.wait_micros.load(Ordering::SeqCst),
        ))
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sample_means_no_signal() {
        assert_eq!(PoolPressure::new().current_wait(), None);
    }

    #[test]
    fn a_fresh_sample_reads_back() {
        let pressure = PoolPressure::new();
        pressure.sample(Duration::from_micros(12_345));
        assert_eq!(pressure.current_wait(), Some(Duration::from_micros(12_345)));
    }

    #[test]
    fn a_stale_sample_is_ignored() {
        let pressure = PoolPressure::new();
        pressure.sample(Duration::from_micros(999_999));
        // Age the sample past the bound without sleeping: rewrite the
        // timestamp directly, which is exactly what the passage of time
        // would have produced.
        pressure.sampled_at_millis.store(
            now_millis().saturating_sub(MAX_SAMPLE_AGE.as_millis() as u64 + 1_000),
            Ordering::SeqCst,
        );
        assert_eq!(pressure.current_wait(), None);
    }
}
