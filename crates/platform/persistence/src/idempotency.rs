//! Idempotency-Key support backed by `platform.idempotency_keys`.
//!
//! Lifecycle: `claim` takes the key (in-progress), the handler does the
//! work, `resolve` stores the response; on a failed attempt `release` frees
//! the key so the client may retry. A client that retries while the original
//! attempt is still running gets the stored response if one exists, or a
//! conflict if the key is still in progress.
//!
//! Every row is identified by `(key, scope)`, not `key` alone: `scope` names
//! the operation an Idempotency-Key was issued for (and, for tenant-scoped
//! endpoints, should fold in the tenant id too, e.g. `"tenant.{id}.settings"`).
//! Without scope in the identity, two callers reusing the same
//! client-supplied key value for different operations — or different
//! tenants — would collide and one would be served the other's response.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{FromRow, PgPool};

/// How long a completed idempotent response stays answerable (clients retry
/// over minutes, not days).
const RESOLVED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// A previously stored response for the same key.
#[derive(Debug, Clone, PartialEq)]
pub struct IdempotencyHit {
    /// HTTP status of the stored response.
    pub status: u16,
    /// Stored response body, if the original attempt completed.
    pub response: Option<Value>,
}

#[derive(Debug, FromRow)]
struct IdempotencyRow {
    response: Option<Value>,
    status: String,
}

#[derive(Debug, Clone)]
pub struct IdempotencyStore {
    pool: PgPool,
}

impl IdempotencyStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Take (or find) a claim on `key`.
    ///
    /// - `None` → the caller owns the key and must do the work.
    /// - `Some(hit)` → replay the stored response instead.
    ///
    /// Expired keys are recycled; keys still marked in-progress by a
    /// concurrent request surface as a conflict.
    pub async fn claim(
        &self,
        key: &str,
        scope: &str,
        ttl: Duration,
    ) -> Result<Option<IdempotencyHit>> {
        if let Some(hit) = self.stored(key, scope).await? {
            return Ok(Some(hit));
        }

        self.recycle_expired(key, scope).await?;

        // Race-safe: the winner inserts; the loser re-reads below.
        let inserted = sqlx::query!(
            "INSERT INTO platform.idempotency_keys (key, scope, response, status, created_at, expires_at)
             VALUES ($1, $2, NULL, 'in_progress', now(), now() + ($3 || ' seconds')::interval)
             ON CONFLICT (key, scope) DO NOTHING",
            key,
            scope,
            ttl.as_secs() as f64
        )
        .execute(&self.pool)
        .await
        .context("failed to claim idempotency key")?
        .rows_affected();

        if inserted > 0 {
            return Ok(None);
        }

        // We lost the race: serve the winner's row.
        self.stored(key, scope).await
    }

    /// Record the final response for `key` in `scope`.
    pub async fn resolve(
        &self,
        key: &str,
        scope: &str,
        status: u16,
        response: Value,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE platform.idempotency_keys
             SET response = $3, status = $4, expires_at = now() + ($5 || ' seconds')::interval
             WHERE key = $1 AND scope = $2",
            key,
            scope,
            response,
            status.to_string(),
            RESOLVED_RETENTION.as_secs() as f64
        )
        .execute(&self.pool)
        .await
        .context("failed to resolve idempotency key")?;
        Ok(())
    }

    /// Free the key after a failed attempt so a retry can claim it again.
    pub async fn release(&self, key: &str, scope: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM platform.idempotency_keys WHERE key = $1 AND scope = $2 AND status = 'in_progress'",
            key,
            scope
        )
        .execute(&self.pool)
        .await
        .context("failed to release idempotency key")?;
        Ok(())
    }

    /// Remove idempotency rows older than `retention` (scheduler sweep).
    pub async fn sweep_expired(&self, retention: Duration) -> Result<u64> {
        let deleted = sqlx::query!(
            "DELETE FROM platform.idempotency_keys WHERE expires_at < now() - ($1 || ' seconds')::interval",
            retention.as_secs() as f64
        )
        .execute(&self.pool)
        .await
        .context("failed to sweep expired idempotency keys")?
        .rows_affected();
        Ok(deleted)
    }

    async fn stored(&self, key: &str, scope: &str) -> Result<Option<IdempotencyHit>> {
        let row = sqlx::query_as!(
            IdempotencyRow,
            "SELECT response, status FROM platform.idempotency_keys
             WHERE key = $1 AND scope = $2 AND expires_at > now()",
            key,
            scope
        )
        .fetch_optional(&self.pool)
        .await
        .context("failed to read idempotency key")?;

        let Some(row) = row else {
            return Ok(None);
        };
        // An unfinished claim is an expected concurrent-request state, not a
        // malformed completed response. Callers can turn its absent response
        // into their operation-specific conflict safely.
        let status = if row.response.is_none() && row.status == "in_progress" {
            409
        } else {
            row.status
                .parse()
                .context("stored idempotency status is not a number")?
        };
        Ok(Some(IdempotencyHit {
            status,
            response: row.response,
        }))
    }

    async fn recycle_expired(&self, key: &str, scope: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM platform.idempotency_keys WHERE key = $1 AND scope = $2 AND expires_at <= now()",
            key,
            scope
        )
        .execute(&self.pool)
        .await
        .context("failed to recycle expired idempotency key")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_constructs_without_connecting() {
        let pool = PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects");
        let _store = IdempotencyStore::new(pool);
    }
}
