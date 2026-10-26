//! Short PostgreSQL leases avoid keeping a pool connection while work runs.

use std::{future::Future, time::Duration};

use anyhow::{Context, Result};
use sanvi_persistence::TxManager;

pub(crate) struct PgLease {
    transactions: TxManager,
    pub(crate) name: String,
    pub(crate) owner: String,
    pub(crate) ttl: Duration,
}

impl PgLease {
    pub(crate) fn new(transactions: TxManager, name: String, owner: String, ttl: Duration) -> Self {
        Self {
            transactions,
            name,
            owner,
            ttl,
        }
    }

    pub(crate) async fn acquire_or_renew(&self) -> Result<bool> {
        let mut tx = self.transactions.begin().await?;
        let ttl_millis = self.ttl.as_millis() as f64;
        let row = sqlx::query_scalar!(
            "INSERT INTO platform.worker_leases (name, owner, expires_at)
             VALUES ($1, $2, clock_timestamp() + $3::float8 * interval '1 millisecond')
             ON CONFLICT (name) DO UPDATE
             SET owner = EXCLUDED.owner, expires_at = EXCLUDED.expires_at
             WHERE platform.worker_leases.owner = EXCLUDED.owner
                OR platform.worker_leases.expires_at <= clock_timestamp()
             RETURNING owner",
            self.name,
            self.owner,
            ttl_millis,
        )
        .fetch_optional(&mut **tx.as_inner())
        .await
        .context("failed to acquire PostgreSQL worker lease")?;
        tx.commit().await?;
        Ok(row.is_some())
    }

    async fn renew(&self) -> Result<()> {
        let mut tx = self.transactions.begin().await?;
        let ttl_millis = self.ttl.as_millis() as f64;
        let updated = sqlx::query!(
            "UPDATE platform.worker_leases
             SET expires_at = clock_timestamp() + $3::float8 * interval '1 millisecond'
             WHERE name = $1 AND owner = $2 AND expires_at > clock_timestamp()",
            self.name,
            self.owner,
            ttl_millis,
        )
        .execute(&mut **tx.as_inner())
        .await
        .context("failed to renew PostgreSQL worker lease")?;
        anyhow::ensure!(updated.rows_affected() == 1, "PostgreSQL worker lease lost");
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn release(&self) -> Result<bool> {
        let mut tx = self.transactions.begin().await?;
        let updated = sqlx::query!(
            "UPDATE platform.worker_leases SET expires_at = clock_timestamp()
             WHERE name = $1 AND owner = $2",
            self.name,
            self.owner,
        )
        .execute(&mut **tx.as_inner())
        .await?;
        tx.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Cancellation or lease loss leaves unfinished durable work available for replay.
    pub(crate) async fn protect<T>(
        &self,
        work: impl Future<Output = Result<T>>,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<T>> {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(None);
        }
        let renew_every = (self.ttl / 3).max(Duration::from_millis(10));
        let keepalive = async {
            loop {
                tokio::time::sleep(renew_every).await;
                // Never let an unavailable database stall lease-loss detection past expiry.
                tokio::time::timeout(renew_every, self.renew())
                    .await
                    .context("PostgreSQL worker lease renewal timed out")??;
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! {
            biased;
            _ = shutdown.changed() => Ok(None),
            result = keepalive => { result?; Ok(None) },
            result = work => result.map(Some),
        }
    }
}
