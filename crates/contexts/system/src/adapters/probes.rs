use std::{sync::Arc, time::Duration};

use sanvi_persistence::PgPool;
use sanvi_persistence::check_database;

use crate::domain::{HealthProbe, SystemError};

/// Readiness probe for Postgres (`SELECT 1` under a budget).
pub struct PostgresProbe {
    pool: PgPool,
}

impl PostgresProbe {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl HealthProbe for PostgresProbe {
    fn name(&self) -> &'static str {
        "database"
    }

    async fn check(&self, budget: Duration) -> Result<(), SystemError> {
        check_database(&self.pool, budget).await.map_err(|err| {
            SystemError::DependencyUnavailable("database".to_string(), err.to_string())
        })
    }
}

/// Readiness probe for Redis (`PING` under a budget).
pub struct RedisProbe {
    conn: Arc<redis::aio::ConnectionManager>,
}

impl RedisProbe {
    pub fn new(conn: Arc<redis::aio::ConnectionManager>) -> Self {
        Self { conn }
    }
}

#[async_trait::async_trait]
impl HealthProbe for RedisProbe {
    fn name(&self) -> &'static str {
        "redis"
    }

    async fn check(&self, budget: Duration) -> Result<(), SystemError> {
        let mut conn = self.conn.as_ref().clone();
        let ping = async { redis::cmd("PING").query_async::<String>(&mut conn).await };
        match tokio::time::timeout(budget, ping).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(SystemError::DependencyUnavailable(
                "redis".to_string(),
                err.to_string(),
            )),
            Err(_) => Err(SystemError::DependencyTimeout("redis".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn postgres_probe_reports_database_name() {
        let pool = PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects");
        assert_eq!(PostgresProbe::new(pool).name(), "database");
    }
}
