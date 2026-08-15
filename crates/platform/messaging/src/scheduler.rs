//! Leader-elected periodic job scheduler.
//!
//! One worker instance holds a Redis lock (`SET NX EX`, renewed on every
//! tick); only the leader runs jobs, so cron-style work (purge sweeps,
//! retention, sync) executes exactly once per interval across the fleet.
//! Jobs run sequentially on the leader; a job that overruns its interval
//! simply runs late (no overlapping runs of the same job).

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use redis::AsyncCommands;
use tracing::{debug, error, info, warn};

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
        }
    }

    /// Run until `shutdown` fires.
    pub async fn run_until_shutdown(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        info!(instance = %self.instance_id, jobs = self.jobs.len(), "scheduler started");
        let mut next_due: Vec<tokio::time::Instant> = self
            .jobs
            .iter()
            .map(|_| tokio::time::Instant::now())
            .collect();

        loop {
            if shutdown.changed().await.is_err() || *shutdown.borrow() {
                break;
            }

            match self.acquire_or_renew().await {
                Ok(true) => {
                    // We are (still) the leader: run due jobs sequentially.
                    let now = tokio::time::Instant::now();
                    for (index, job) in self.jobs.iter().enumerate() {
                        if next_due[index] <= now {
                            debug!(job = job.name(), "running scheduled job");
                            if let Err(err) = job.run().await {
                                error!(error = %err, job = job.name(), "scheduled job failed");
                            }
                            next_due[index] = now + job.interval();
                        }
                    }
                }
                Ok(false) => {
                    debug!("scheduler is not the leader; waiting");
                }
                Err(err) => {
                    warn!(error = %err, "scheduler lock check failed; waiting");
                }
            }

            tokio::time::sleep(TICK).await;
        }
        info!("scheduler stopped");
    }

    /// Take the leader lock, or extend it if we already hold it.
    async fn acquire_or_renew(&self) -> Result<bool> {
        let mut conn = self.conn.clone();
        // SET NX succeeds only when nobody holds the lock; when we already
        // hold it the key matches our instance id, in which case EXPIRE
        // (kept-live) is enough.
        let acquired: Option<String> = conn
            .set_options(
                &self.lock_key,
                &self.instance_id,
                redis::SetOptions::default()
                    .conditional_set(redis::ExistenceCheck::NX)
                    .with_expiration(redis::SetExpiry::EX(LOCK_TTL_SECS)),
            )
            .await
            .context("failed to take scheduler leader lock")?;

        if acquired.is_some() {
            return Ok(true);
        }

        let current: Option<String> = conn
            .get(&self.lock_key)
            .await
            .context("failed to read scheduler leader lock")?;
        if current.as_deref() == Some(self.instance_id.as_str()) {
            let _: () = conn
                .expire(&self.lock_key, LOCK_TTL_SECS as i64)
                .await
                .context("failed to renew scheduler leader lock")?;
            Ok(true)
        } else {
            Ok(false)
        }
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
