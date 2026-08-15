use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::PgPool;
use tracing::info;

/// Runs SQL migrations from a directory of timestamp-prefixed files.
///
/// Loaded at runtime (not via `sqlx::migrate!`) so the same binary works in
/// containers and on laptops without recompiling; the files ship inside the
/// container image.
#[derive(Debug, Clone)]
pub struct Migrator {
    path: PathBuf,
}

impl Migrator {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    async fn load(&self) -> Result<sqlx::migrate::Migrator> {
        sqlx::migrate::Migrator::new(Path::new(&self.path))
            .await
            .with_context(|| format!("failed to load migrations from {}", self.path.display()))
    }

    /// Apply all pending migrations.
    pub async fn run(&self, pool: &PgPool) -> Result<()> {
        let migrator = self.load().await?;
        info!(dir = %self.path.display(), "applying migrations");
        migrator
            .run(pool)
            .await
            .context("migration run failed (is the migrator role in use?)")?;
        info!("migrations up to date");
        Ok(())
    }

    /// Revert migrations down to (and including) `target`.
    ///
    /// `target = 0` reverts everything. Used by rollbacks; CI asserts every
    /// migration is reversible by exercising this path.
    pub async fn undo(&self, pool: &PgPool, target: i64) -> Result<()> {
        let migrator = self.load().await?;
        info!(dir = %self.path.display(), target, "reverting migrations");
        migrator
            .undo(pool, target)
            .await
            .context("migration revert failed")?;
        Ok(())
    }

    /// List applied migrations in order.
    pub async fn applied(&self, pool: &PgPool) -> Result<Vec<(i64, String)>> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT version, description FROM _sqlx_migrations ORDER BY version")
                .fetch_all(pool)
                .await
                .context("failed to read applied migrations (schema may be missing)")?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_preserved() {
        let migrator = Migrator::new("/tmp/migrations");
        assert_eq!(migrator.path, PathBuf::from("/tmp/migrations"));
    }
}
