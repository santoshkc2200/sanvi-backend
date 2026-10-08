//! sqlx pool gauges, so pool saturation is measured rather than guessed
//! (TASK-021 step 0: the 2026-09-17 baseline's pool section is empty for
//! exactly this reason, and step 5 sizes the pools from these numbers).
//!
//! sqlx exposes `size()` and `num_idle()` but no acquire-wait hook, and read
//! paths check connections out of the pool directly, so per-acquire
//! instrumentation would mean touching every repository. The sampler measures
//! what a fresh acquire experiences instead: once per interval it probes the
//! pool with one real acquire and records how long that waited. Under
//! saturation the probe waits behind the queue — which is the number pool
//! sizing needs. The probe is one extra connection demand at 1 Hz: noise
//! against any workload worth measuring.

use std::sync::Arc;
use std::time::{Duration, Instant};

use prometheus::{Histogram, HistogramOpts, IntCounter, IntGauge, Registry};
use sqlx::PgPool;
use tracing::debug;

/// How often the sampler reads the pool and probes it. Fast enough that a
/// saturation episode lasting a few seconds is sampled at all; slow enough
/// that the probe itself is never part of the load.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Pool gauges registered on the process Prometheus registry, plus the
/// acquire-probe instruments the sampler feeds.
#[derive(Clone)]
pub struct PoolMetrics {
    /// Connections currently open (busy + idle).
    pub size: IntGauge,
    /// Open connections sitting idle in the pool.
    pub idle: IntGauge,
    /// Wait a probe acquire experienced before checking out a connection.
    /// Near zero with headroom; climbing towards the acquire timeout means
    /// the pool is the bottleneck.
    pub acquire_seconds: Histogram,
    /// Probe acquires that failed (pool timeout or connection error) —
    /// under a healthy database this stays at zero.
    pub acquire_failures: IntCounter,
    /// The load shedder's live pressure signal (TASK-023, FR-1114): fed by
    /// the sampler, read on admission. `None` until the composition root
    /// wires it — the gauges above are unaffected either way.
    pressure: Option<std::sync::Arc<sanvi_resilience::PoolPressure>>,
}

impl PoolMetrics {
    pub fn new(registry: &Registry) -> Self {
        let size = IntGauge::new(
            "db_pool_size_gauge",
            "Open Postgres connections (busy + idle)",
        )
        .expect("gauge construction is infallible");
        let idle = IntGauge::new(
            "db_pool_idle_gauge",
            "Open Postgres connections sitting idle in the pool",
        )
        .expect("gauge construction is infallible");
        let acquire_seconds = Histogram::with_opts(HistogramOpts::new(
            "db_pool_acquire_seconds",
            "Seconds a probe acquire waited before checking out a connection",
        ))
        .expect("histogram construction is infallible");
        let acquire_failures = IntCounter::new(
            "db_pool_acquire_failures_total",
            "Probe acquires that failed to check out a connection",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(size.clone()));
        let _ = registry.register(Box::new(idle.clone()));
        let _ = registry.register(Box::new(acquire_seconds.clone()));
        let _ = registry.register(Box::new(acquire_failures.clone()));
        Self {
            size,
            idle,
            acquire_seconds,
            acquire_failures,
            pressure: None,
        }
    }

    /// Wire the shedder's live pressure signal. The composition root calls
    /// this once; every probe sample then feeds the signal the load
    /// shedder reads on admission.
    pub fn with_pressure(
        mut self,
        pressure: std::sync::Arc<sanvi_resilience::PoolPressure>,
    ) -> Self {
        self.pressure = Some(pressure);
        self
    }

    /// Read the pool once: gauges from the pool's own counters, one probe
    /// acquire for the wait histogram. The spawned sampler calls this every
    /// [`SAMPLE_INTERVAL`]; tests call it directly.
    pub async fn sample_once(&self, pool: &PgPool) {
        self.size.set(pool.size() as i64);
        self.idle.set(pool.num_idle() as i64);

        let started = Instant::now();
        match pool.acquire().await {
            Ok(connection) => {
                let waited = started.elapsed();
                self.acquire_seconds.observe(waited.as_secs_f64());
                if let Some(pressure) = &self.pressure {
                    pressure.sample(waited);
                }
                drop(connection);
            }
            Err(err) => {
                self.acquire_failures.inc();
                // A failed probe waited the whole acquire timeout behind
                // the queue: record the wait so the shedder sees the
                // saturation instead of a missing sample.
                if let Some(pressure) = &self.pressure {
                    pressure.sample(started.elapsed());
                }
                debug!(error = %err, "pool acquire probe failed");
            }
        }
    }

    /// Start the background sampler for a process pool. The task runs for
    /// the process lifetime (detached); the gauges carry values from the
    /// first interval on, with the initial read issued immediately so an
    /// early scrape is not empty.
    pub fn spawn_sampler(self: Arc<Self>, pool: PgPool) -> tokio::task::JoinHandle<()> {
        let pool_for_startup = pool.clone();
        tokio::spawn(async move {
            self.sample_once(&pool_for_startup).await;
            let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
            loop {
                ticker.tick().await;
                self.sample_once(&pool).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::new()
    }

    #[test]
    fn registers_all_four_instruments_on_the_registry() {
        let registry = registry();
        let _metrics = PoolMetrics::new(&registry);
        let rendered = prometheus::TextEncoder::new()
            .encode_to_string(&registry.gather())
            .unwrap();
        for name in [
            "db_pool_size_gauge",
            "db_pool_idle_gauge",
            "db_pool_acquire_seconds",
            "db_pool_acquire_failures_total",
        ] {
            assert!(rendered.contains(name), "missing {name} in {rendered}");
        }
    }

    #[tokio::test]
    async fn gauges_reflect_the_pool_without_connecting() {
        let registry = registry();
        let metrics = PoolMetrics::new(&registry);
        let pool = dead_pool();
        metrics.sample_once(&pool).await;

        let rendered = prometheus::TextEncoder::new()
            .encode_to_string(&registry.gather())
            .unwrap();
        assert!(
            rendered.contains("db_pool_size_gauge 0"),
            "a lazy pool holds no connections: {rendered}"
        );
        assert!(
            rendered.contains("db_pool_idle_gauge 0"),
            "a lazy pool holds no idle connections: {rendered}"
        );
        assert!(
            !rendered.contains("db_pool_acquire_seconds_count 1"),
            "nothing was acquired yet, so the probe must not have observed: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_failed_acquire_lands_in_the_failure_counter() {
        let registry = registry();
        let metrics = PoolMetrics::new(&registry);
        // Nothing listens on port 1: the probe cannot check out a
        // connection, which is the saturation-adjacent signal the counter
        // exists for.
        let pool = dead_pool();
        metrics.sample_once(&pool).await;

        let rendered = prometheus::TextEncoder::new()
            .encode_to_string(&registry.gather())
            .unwrap();
        assert!(
            rendered.contains("db_pool_acquire_failures_total 1"),
            "the failed probe must be counted: {rendered}"
        );
    }

    /// A lazy pool aimed at a port nothing listens on. The acquire timeout
    /// must be short — sqlx keeps retrying the connect until the budget
    /// expires, and the default 30 s would stall every test that probes it.
    fn dead_pool() -> PgPool {
        sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
            .acquire_timeout(Duration::from_millis(500))
            .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects")
    }
}
