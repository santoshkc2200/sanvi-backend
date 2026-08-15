use anyhow::{Context, Result};
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

/// Transaction handle. Every write path in the system runs inside one.
///
/// The manager also owns the row-level-security context: `set_tenant_id`
/// issues `SET LOCAL sanvi.tenant_id` so tenant-scoped reads are filtered by
/// Postgres itself, not by ad-hoc `WHERE` clauses.
pub struct Tx<'a> {
    inner: sqlx::Transaction<'a, sqlx::Postgres>,
}

impl<'a> Tx<'a> {
    /// Scope this transaction to a tenant for RLS filtering.
    pub async fn set_tenant_id(&mut self, tenant_id: Uuid) -> Result<()> {
        sqlx::query("SELECT set_config('sanvi.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *self.inner)
            .await
            .context("failed to set tenant id for RLS")?;
        Ok(())
    }

    /// Cap statement runtime inside this transaction.
    pub async fn set_statement_timeout_ms(&mut self, millis: u64) -> Result<()> {
        sqlx::query("SELECT set_config('statement_timeout', $1, true)")
            .bind(millis.to_string())
            .execute(&mut *self.inner)
            .await
            .context("failed to set statement timeout")?;
        Ok(())
    }

    pub async fn commit(self) -> Result<()> {
        self.inner
            .commit()
            .await
            .context("failed to commit transaction")
    }

    pub async fn rollback(self) -> Result<()> {
        self.inner
            .rollback()
            .await
            .context("failed to roll back transaction")
    }

    /// Access the underlying transaction (driven adapters use this).
    pub fn as_inner(&mut self) -> &mut sqlx::Transaction<'a, sqlx::Postgres> {
        &mut self.inner
    }
}

/// Opens read/write transactions on the shared pool.
#[derive(Debug, Clone)]
pub struct TxManager {
    pool: PgPool,
}

impl TxManager {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn begin(&self) -> Result<Tx<'_>> {
        let inner = self
            .pool
            .begin()
            .await
            .context("failed to begin transaction (pool exhausted or database unavailable)")?;
        // debug, not info: every write path opens one of these, so at info
        // level this would spam a log line per request in production.
        debug!("transaction started");
        Ok(Tx { inner })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manager_wraps_a_pool() {
        // No live database needed: the manager is just a handle.
        let pool = PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects");
        let manager = TxManager::new(pool);
        assert_eq!(manager.pool().size(), 0);
    }
}
