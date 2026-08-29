use std::sync::Arc;

use anyhow::{Context, Result};
use sanvi_kernel::RawIdGenerator;
use sqlx::PgPool;
use tracing::debug;

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
    pub async fn set_tenant_id(&mut self, tenant_id: i64) -> Result<()> {
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

/// A transaction scoped to exactly one tenant.
///
/// Constructed only by [`TxManager::begin_for`], which issues
/// `SET LOCAL sanvi.tenant_id` before returning — so any repository port that
/// takes a `&mut ScopedTx` (tenant-scoped tables only) *cannot* be reached
/// without the RLS context. Forgetting the scope is a type error, not a
/// row leak.
pub struct ScopedTx<'a> {
    inner: Tx<'a>,
}

impl<'a> ScopedTx<'a> {
    pub async fn commit(self) -> Result<()> {
        self.inner.commit().await
    }

    pub async fn rollback(self) -> Result<()> {
        self.inner.rollback().await
    }

    /// The raw transaction (outbox writer and similar platform machinery).
    pub fn tx_mut(&mut self) -> &mut Tx<'a> {
        &mut self.inner
    }

    /// Cap statement runtime inside this transaction.
    pub async fn set_statement_timeout_ms(&mut self, millis: u64) -> Result<()> {
        self.inner.set_statement_timeout_ms(millis).await
    }
}

/// A deliberately unscoped (platform-wide) transaction.
///
/// Constructed only by [`TxManager::begin_unscoped`], which writes an audit
/// row recording the reason and actor *before* any statement can run. This is
/// the single escape hatch from tenant scoping, and every use is on the
/// record.
pub struct PlatformScope<'a> {
    inner: Tx<'a>,
}

impl<'a> PlatformScope<'a> {
    pub async fn commit(self) -> Result<()> {
        self.inner.commit().await
    }

    pub async fn rollback(self) -> Result<()> {
        self.inner.rollback().await
    }

    /// The raw transaction (outbox writer and similar platform machinery).
    pub fn tx_mut(&mut self) -> &mut Tx<'a> {
        &mut self.inner
    }

    /// Cap statement runtime inside this transaction.
    pub async fn set_statement_timeout_ms(&mut self, millis: u64) -> Result<()> {
        self.inner.set_statement_timeout_ms(millis).await
    }
}

/// Opens read/write transactions on the shared pool.
#[derive(Clone)]
pub struct TxManager {
    pool: PgPool,
    ids: Arc<dyn RawIdGenerator>,
}

impl TxManager {
    pub fn new(pool: PgPool, ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { pool, ids }
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

    /// Open a tenant-scoped transaction: `SET LOCAL sanvi.tenant_id` runs
    /// before anything else, so every tenant-scoped table in the transaction
    /// is filtered by Postgres itself.
    pub async fn begin_for(&self, tenant_id: i64) -> Result<ScopedTx<'_>> {
        let mut tx = self.begin().await?;
        tx.set_tenant_id(tenant_id).await?;
        debug!(tenant_id = %tenant_id, "tenant-scoped transaction started");
        Ok(ScopedTx { inner: tx })
    }

    /// Open an audited, unscoped transaction (platform-wide access).
    ///
    /// `reason` and `actor` are recorded in `platform.audit_log` inside the
    /// same transaction, so the audit trail commits atomically with whatever
    /// the caller does next.
    ///
    /// The transaction also sets `sanvi.platform_scope`, the audited
    /// escape hatch in the shared RLS policy: tenant-scoped tables admit
    /// platform-scoped reads/writes only when this GUC is on, so
    /// cross-tenant maintenance (reconciliation, seeding) stays confined to
    /// code paths that reached this audited entry point.
    pub async fn begin_unscoped(&self, reason: &str, actor: &str) -> Result<PlatformScope<'_>> {
        let mut tx = self.begin().await?;
        let audit_id = self
            .ids
            .next_id()
            .context("failed to generate platform audit id")?;
        sqlx::query(
            "INSERT INTO platform.audit_log (id, actor, action, reason, detail)
             VALUES ($1, $2, 'platform_scope_begin', $3, '{}'::jsonb)",
        )
        .bind(audit_id.as_i64())
        .bind(actor)
        .bind(reason)
        .execute(&mut **tx.as_inner())
        .await
        .context("failed to write platform scope audit record")?;
        sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
            .execute(&mut **tx.as_inner())
            .await
            .context("failed to set platform scope for RLS bypass")?;
        debug!(reason, actor, "unscoped platform transaction started");
        Ok(PlatformScope { inner: tx })
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
        let manager = TxManager::new(pool, Arc::new(TestIds));
        assert_eq!(manager.pool().size(), 0);
    }

    #[derive(Debug)]
    struct TestIds;

    impl RawIdGenerator for TestIds {
        fn next_id(&self) -> Result<sanvi_kernel::SnowflakeId, sanvi_kernel::IdGenerationError> {
            sanvi_kernel::SnowflakeId::try_from(1)
                .map_err(|_| sanvi_kernel::IdGenerationError::OutOfRange)
        }
    }
}
