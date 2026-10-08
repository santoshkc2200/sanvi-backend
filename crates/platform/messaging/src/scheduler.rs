//! Leader-elected periodic job scheduler.
//!
//! One worker instance holds a Redis lock (`SET NX EX`, renewed on every
//! tick); only the leader runs jobs, so cron-style work (purge sweeps,
//! retention, sync) executes exactly once per interval across the fleet.
//! Jobs run sequentially on the leader; a job that overruns its interval
//! simply runs late (no overlapping runs of the same job).

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use sanvi_resilience::{JobRetryBudget, JobRetryPolicy, RetryOutcome};
use tracing::{debug, error, info, warn};

static ACQUIRE_OR_RENEW_SCRIPT: std::sync::LazyLock<redis::Script> =
    std::sync::LazyLock::new(|| {
        redis::Script::new(
            r#"
        local current = redis.call('GET', KEYS[1])
        if not current then
            redis.call('SET', KEYS[1], ARGV[1], 'EX', tonumber(ARGV[2]))
            return 1
        elseif current == ARGV[1] then
            redis.call('EXPIRE', KEYS[1], tonumber(ARGV[2]))
            return 1
        else
            return 0
        end
        "#,
        )
    });

static RELEASE_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        r#"
        if redis.call('GET', KEYS[1]) == ARGV[1] then
            return redis.call('DEL', KEYS[1])
        else
            return 0
        end
        "#,
    )
});

/// Boxed job future.
pub type JobFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// A periodic job with its own interval.
pub trait Job: Send + Sync {
    fn name(&self) -> &str;
    fn interval(&self) -> Duration;
    fn run(&self) -> JobFuture;
}

/// A job defined by a name, an interval and an async closure.
pub struct FnJob {
    name: &'static str,
    interval: Duration,
    run: Arc<dyn Fn() -> JobFuture + Send + Sync>,
}

impl FnJob {
    pub fn new(
        name: &'static str,
        interval: Duration,
        run: impl Fn() -> JobFuture + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            interval,
            run: Arc::new(run),
        }
    }
}

impl Job for FnJob {
    fn name(&self) -> &str {
        self.name
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn run(&self) -> JobFuture {
        (self.run)()
    }
}

/// Lease duration for the leader lock (renewed every tick).
const LOCK_TTL_SECS: u64 = 60;
/// Tick cadence: how often jobs are checked for due-ness.
const TICK: Duration = Duration::from_secs(5);

pub struct Scheduler {
    conn: redis::aio::ConnectionManager,
    lock_key: String,
    instance_id: String,
    jobs: Vec<Arc<dyn Job>>,
    lock_ttl_secs: u64,
    tick: Duration,
    /// One retry budget per job name (TASK-023 step 7): a permanently
    /// failing job suspends at its budget instead of burning capacity
    /// forever, until the skip window lapses (one probe run) or the
    /// operator replays it.
    retry_policy: JobRetryPolicy,
    budgets: std::sync::Mutex<HashMap<String, JobRetryBudget>>,
}

impl Scheduler {
    pub fn new(
        conn: redis::aio::ConnectionManager,
        instance_id: impl Into<String>,
        jobs: Vec<Arc<dyn Job>>,
    ) -> Self {
        Self {
            conn,
            lock_key: "sanvi:scheduler:leader".to_string(),
            instance_id: instance_id.into(),
            jobs,
            lock_ttl_secs: LOCK_TTL_SECS,
            tick: TICK,
            retry_policy: JobRetryPolicy::default(),
            budgets: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn with_lock_key(mut self, lock_key: impl Into<String>) -> Self {
        self.lock_key = lock_key.into();
        self
    }

    pub fn with_lock_ttl(mut self, ttl_secs: u64) -> Self {
        self.lock_ttl_secs = ttl_secs;
        self
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Bound every job's consecutive failures: past the budget the job
    /// suspends (skipped with a warning) instead of running forever.
    pub fn with_retry_policy(mut self, policy: JobRetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Run until `shutdown` fires.
    pub async fn run_until_shutdown(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        info!(instance = %self.instance_id, jobs = self.jobs.len(), "scheduler started");
        if *shutdown.borrow() {
            info!("scheduler stopped: already shut down");
            return;
        }

        let mut next_due: Vec<tokio::time::Instant> = self
            .jobs
            .iter()
            .map(|_| tokio::time::Instant::now())
            .collect();

        let mut ticker = tokio::time::interval(self.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
            }

            if *shutdown.borrow() {
                break;
            }

            match self.acquire_or_renew().await {
                Ok(true) => {
                    // We are (still) the leader: run due jobs with lease renewal.
                    self.run_due_jobs(&mut next_due, &mut shutdown).await;
                    if *shutdown.borrow() {
                        break;
                    }
                }
                Ok(false) => {
                    debug!("scheduler is not the leader; waiting");
                }
                Err(err) => {
                    warn!(error = %err, "scheduler lock check failed; waiting");
                }
            }
        }

        if let Err(err) = self.release_lock().await {
            warn!(error = %err, "failed to release scheduler lock on shutdown");
        }
        info!("scheduler stopped");
    }

    async fn run_due_jobs(
        &self,
        next_due: &mut [tokio::time::Instant],
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) {
        let now = tokio::time::Instant::now();
        let due_indices: Vec<usize> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(index, _)| next_due[*index] <= now)
            .map(|(index, _)| index)
            .collect();

        if due_indices.is_empty() {
            return;
        }

        // Renew frequently enough that a sequence outliving the lease cannot cause
        // lease expiry. We renew at min(tick, 1/3 TTL), bounded to at least 10ms.
        let ttl_third = Duration::from_millis(((self.lock_ttl_secs * 1000) / 3).max(10));
        let renew_interval = self.tick.min(ttl_third).max(Duration::from_millis(10));

        let mut renew_interval_timer = tokio::time::interval(renew_interval);
        renew_interval_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick completes immediately, consume it since lock was just acquired/renewed
        renew_interval_timer.tick().await;

        let keepalive = async {
            loop {
                renew_interval_timer.tick().await;
                match self.acquire_or_renew().await {
                    Ok(true) => {
                        debug!(instance = %self.instance_id, "renewed leader lease during job execution");
                    }
                    Ok(false) => {
                        warn!(instance = %self.instance_id, "lost leader lease during job execution");
                        return Err(anyhow::anyhow!("lease lost to another worker"));
                    }
                    Err(err) => {
                        warn!(error = %err, instance = %self.instance_id, "failed to renew leader lease during job execution");
                        return Err(err);
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };

        let shutdown_jobs = shutdown.clone();
        let jobs_runner = async {
            for index in due_indices {
                if *shutdown_jobs.borrow() {
                    break;
                }
                let job = &self.jobs[index];
                if !self.admit(job.name()) {
                    continue;
                }
                debug!(job = job.name(), "running scheduled job");
                match job.run().await {
                    Ok(()) => self.record_success(job.name()),
                    Err(err) => {
                        error!(error = %err, job = job.name(), "scheduled job failed");
                        self.record_failure(job.name());
                    }
                }
                next_due[index] = tokio::time::Instant::now() + job.interval();
            }
        };

        tokio::select! {
            biased;

            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    info!("shutdown signaled during job execution; cancelling remaining jobs");
                }
            }
            lease_res = keepalive => {
                if let Err(err) = lease_res {
                    error!(error = %err, "lost leadership lease during job sequence; cancelled running jobs");
                }
            }
            _ = jobs_runner => {
                debug!("completed due jobs sequence");
            }
        }
    }

    /// May this job run on this tick? A suspended job is skipped with a
    /// warning — the operator path back is the DLQ/CLI replay surface, or
    /// the probe run the budget allows once the skip window lapses.
    fn admit(&self, job_name: &str) -> bool {
        let mut budgets = self.budgets.lock().expect("scheduler budget lock");
        let budget = budgets
            .entry(job_name.to_string())
            .or_insert_with(|| JobRetryBudget::new(self.retry_policy));
        match budget.admit() {
            RetryOutcome::Run => true,
            RetryOutcome::SkipUntil(until) => {
                warn!(
                    job = job_name,
                    resume_at = ?until,
                    "scheduled job suspended at its retry budget; skipping"
                );
                false
            }
        }
    }

    fn record_success(&self, job_name: &str) {
        let budgets = self.budgets.lock().expect("scheduler budget lock");
        if let Some(budget) = budgets.get(job_name) {
            budget.record_success();
        }
    }

    fn record_failure(&self, job_name: &str) {
        let mut budgets = self.budgets.lock().expect("scheduler budget lock");
        let budget = budgets
            .entry(job_name.to_string())
            .or_insert_with(|| JobRetryBudget::new(self.retry_policy));
        budget.record_failure();
    }

    /// Take the leader lock, or extend it if we already hold it.
    ///
    /// Atomic single round trip using Lua compare-and-set:
    /// - If nobody holds the lock, claims it with TTL.
    /// - If this instance already holds the lock, refreshes the TTL.
    /// - If another instance holds the lock, returns `Ok(false)` without modifying it.
    pub async fn acquire_or_renew(&self) -> Result<bool> {
        let mut conn = self.conn.clone();
        let res: i64 = ACQUIRE_OR_RENEW_SCRIPT
            .key(&self.lock_key)
            .arg(&self.instance_id)
            .arg(self.lock_ttl_secs)
            .invoke_async(&mut conn)
            .await
            .context("failed to acquire or renew scheduler leader lock")?;
        Ok(res == 1)
    }

    /// Release the leader lock if held by this instance.
    pub async fn release_lock(&self) -> Result<bool> {
        let mut conn = self.conn.clone();
        let res: i64 = RELEASE_SCRIPT
            .key(&self.lock_key)
            .arg(&self.instance_id)
            .invoke_async(&mut conn)
            .await
            .context("failed to release scheduler leader lock")?;
        Ok(res == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fn_job_carries_its_contract() {
        let job = FnJob::new("sweep", Duration::from_secs(60), || {
            Box::pin(async { Ok(()) })
        });
        assert_eq!(job.name(), "sweep");
        assert_eq!(job.interval(), Duration::from_secs(60));
    }
}
