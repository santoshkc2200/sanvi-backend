use std::time::Duration;

use sqlx::PgPool;
use thiserror::Error;

/// Failures from the database health probe.
#[derive(Debug, Error)]
pub enum DatabaseHealthError {
    #[error("database health check timed out after {0:?}")]
    Timeout(Duration),
    #[error("database health check failed: {0}")]
    Unavailable(#[from] sqlx::Error),
}

/// Cheap liveness-style check used by `/readyz`.
///
/// Runs `SELECT 1` under a time budget so an unhealthy database degrades the
/// readiness response instead of hanging it.
pub async fn check_database(pool: &PgPool, budget: Duration) -> Result<(), DatabaseHealthError> {
    let check = async { sqlx::query("SELECT 1").execute(pool).await.map(|_| ()) };

    tokio::time::timeout(budget, check)
        .await
        .map_err(|_| DatabaseHealthError::Timeout(budget))?
        .map_err(DatabaseHealthError::Unavailable)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unreachable_database_is_reported_as_unavailable() {
        let pool = PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects");
        let result = check_database(&pool, Duration::from_millis(250)).await;
        assert!(matches!(
            result,
            Err(DatabaseHealthError::Unavailable(_) | DatabaseHealthError::Timeout(_))
        ));
    }
}
