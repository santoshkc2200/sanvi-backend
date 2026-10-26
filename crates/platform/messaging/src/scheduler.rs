//! PostgreSQL-backed recurring jobs with durable schedules and retry state.
//!
//! One renewable PostgreSQL lease coordinates a scheduler fleet. Completion
//! advances the persisted due time; interrupted work stays due for replay.
//! Jobs must be idempotent because a crash after an external effect can replay it.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use sanvi_persistence::TxManager;
use sanvi_resilience::JobRetryPolicy;
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::lease::PgLease;

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

pub struct Scheduler {
    pool: PgPool,
    transactions: TxManager,
    lease: PgLease,
    jobs: Vec<Arc<dyn Job>>,
    tick: Duration,
    retry_policy: JobRetryPolicy,
}

impl Scheduler {
    pub fn new(
        transactions: TxManager,
        instance_id: impl Into<String>,
        jobs: Vec<Arc<dyn Job>>,
    ) -> Self {
        let pool = transactions.pool().clone();
        Self {
            lease: PgLease::new(
                transactions.clone(),
                "sanvi:scheduler:leader".into(),
                instance_id.into(),
                Duration::from_secs(60),
            ),
            pool,
            transactions,
            jobs,
            tick: Duration::from_secs(5),
            retry_policy: JobRetryPolicy::default(),
        }
    }

    pub fn with_lock_key(mut self, key: impl Into<String>) -> Self {
        self.lease.name = key.into();
        self
    }

    pub fn with_lock_ttl(mut self, ttl_secs: u64) -> Self {
        self.lease.ttl = Duration::from_secs(ttl_secs.max(1));
        self
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick.max(Duration::from_millis(1));
        self
    }

    pub fn with_retry_policy(mut self, policy: JobRetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    pub async fn acquire_or_renew(&self) -> Result<bool> {
        self.lease.acquire_or_renew().await
    }

    pub async fn release_lock(&self) -> Result<bool> {
        self.lease.release().await
    }

    /// Jobs run sequentially; failover preserves due times and retry budgets.
    pub async fn run_until_shutdown(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return;
        }
        info!(instance = %self.lease.owner, jobs = self.jobs.len(), "PostgreSQL scheduler started");
        let mut ticker = tokio::time::interval(self.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {},
                _ = shutdown.changed() => break,
            }
            if *shutdown.borrow() {
                break;
            }
            match self.acquire_or_renew().await {
                Ok(true) => match self.lease.protect(self.run_due_jobs(), &mut shutdown).await {
                    Ok(Some(())) => {}
                    Ok(None) => break,
                    Err(err) => {
                        warn!(error = %err, "scheduled jobs interrupted; pending work will retry")
                    }
                },
                Ok(false) => {}
                Err(err) => warn!(error = %err, "scheduler lease check failed"),
            }
        }
        if let Err(err) = self.release_lock().await {
            warn!(error = %err, "failed to release scheduler lease");
        }
        info!("PostgreSQL scheduler stopped");
    }

    async fn run_due_jobs(&self) -> Result<()> {
        // One query per tick. A job is due at its persisted time, or one current
        // interval after its last finish when it is not backing off, so a
        // shortened interval applies without waiting out the old delay.
        let names: Vec<&str> = self.jobs.iter().map(|job| job.name()).collect();
        let interval_millis: Vec<f64> = self
            .jobs
            .iter()
            .map(|job| job.interval().as_millis() as f64)
            .collect();
        let due: std::collections::HashMap<String, Option<i32>> = sqlx::query_as(
            "SELECT c.name, j.consecutive_failures
             FROM unnest($2::text[], $3::float8[]) AS c(name, interval_millis)
             LEFT JOIN platform.scheduled_jobs j ON j.scheduler_key = $1 AND j.name = c.name
             WHERE j.name IS NULL
                OR j.next_run_at <= clock_timestamp()
                OR (j.consecutive_failures < $4
                    AND j.last_finished_at + c.interval_millis * interval '1 millisecond'
                        <= clock_timestamp())",
        )
        .bind(&self.lease.name)
        .bind(&names)
        .bind(&interval_millis)
        .bind(self.retry_policy.max_consecutive_failures as i32)
        .fetch_all(&self.pool)
        .await
        .context("failed to read due scheduled jobs")?
        .into_iter()
        .collect();
        for job in &self.jobs {
            let Some(failures) = due.get(job.name()) else {
                continue;
            };
            if failures.is_none()
                && let Err(err) = self.register(job.name()).await
            {
                warn!(error = %err, job = job.name(), "failed to register scheduled job");
                continue;
            }
            let failures = failures.unwrap_or(0);
            let (failures, last_error, delay) = match job.run().await {
                Ok(()) => (0, None, job.interval()),
                Err(err) => {
                    let failures = failures.saturating_add(1);
                    error!(error = %err, job = job.name(), failures, "scheduled job failed");
                    let delay = if failures as u32 >= self.retry_policy.max_consecutive_failures {
                        self.retry_policy
                            .skip_window(failures as u32)
                            .max(job.interval())
                    } else {
                        job.interval()
                    };
                    (failures, Some(err.to_string()), delay)
                }
            };
            // Fencing prevents an expired leader from advancing durable schedule state.
            let delay_millis = delay.as_millis() as f64;
            let mut completion = self.transactions.begin().await?;
            let updated = sqlx::query!(
                "UPDATE platform.scheduled_jobs
                 SET next_run_at = clock_timestamp() + $3::float8 * interval '1 millisecond',
                     consecutive_failures = $4, last_error = $5, last_finished_at = clock_timestamp()
                 WHERE scheduler_key = $1 AND name = $2 AND EXISTS (
                     SELECT 1 FROM platform.worker_leases
                     WHERE name = $1 AND owner = $6 AND expires_at > clock_timestamp())",
                self.lease.name, job.name(), delay_millis, failures, last_error, self.lease.owner,
            )
            .execute(&mut **completion.as_inner()).await.context("failed to persist scheduled job outcome")?;
            anyhow::ensure!(
                updated.rows_affected() == 1,
                "scheduler lease lost before recording job outcome"
            );
            completion.commit().await?;
        }
        Ok(())
    }

    async fn register(&self, name: &str) -> Result<()> {
        let mut registration = self.transactions.begin().await?;
        sqlx::query!(
            "INSERT INTO platform.scheduled_jobs (scheduler_key, name) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
            self.lease.name,
            name,
        )
        .execute(&mut **registration.as_inner())
        .await?;
        registration.commit().await?;
        Ok(())
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
