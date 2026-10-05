use anyhow::{Context, Result};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tracing::info;

use crate::config::{DatabaseConfig, SslMode};

/// Build the application connection pool with production-sane limits.
pub async fn build_pool(config: &DatabaseConfig) -> Result<PgPool> {
    config.validate()?;

    let mut options = PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .database(&config.name)
        .username(&config.user)
        .application_name("sanvi");

    if !config.password.is_empty() {
        options = options.password(&config.password);
    }

    options = match config.ssl_mode {
        SslMode::Disable => options.ssl_mode(sqlx::postgres::PgSslMode::Disable),
        SslMode::Require => options.ssl_mode(sqlx::postgres::PgSslMode::Require),
    };

    // Per-connection statement cap (TASK-021, FR-1110): the startup parameter
    // form applies to every pooled connection for the pool's lifetime. `0`
    // leaves the server default in place — the migration pools take that
    // branch because DDL against scale data outlives any request budget.
    if config.statement_timeout_ms > 0 {
        let timeout = config.statement_timeout_ms.to_string();
        options = options.options([("statement_timeout", timeout)]);
    }

    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(config.acquire_timeout())
        .idle_timeout(config.idle_timeout())
        .max_lifetime(config.max_lifetime())
        .connect_with(options)
        .await
        .context("failed to connect to postgres")?;

    info!(
        host = %config.host,
        port = config.port,
        database = %config.name,
        user = %config.user,
        statement_timeout_ms = config.statement_timeout_ms,
        "connected to postgres",
    );

    Ok(pool)
}

/// Whether `err` is Postgres cancelling the statement because
/// `statement_timeout` elapsed (SQLSTATE 57014 with the statement-timeout
/// message). The message check keeps lock timeouts — same SQLSTATE, different
/// budget — out, because they are not this contract's class.
///
/// Adapters flatten sqlx errors into domain `Storage(String)` variants before
/// handlers see them, so HTTP-side classification matches the rendered
/// message instead (`sanvi_http::ApiError::from_storage_detail`); this
/// predicate is the typed form for anything still holding the `sqlx::Error`.
pub fn is_statement_timeout(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db) => {
            db.code().as_deref() == Some("57014")
                && db.message().contains("due to statement timeout")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_config_is_rejected_before_connecting() {
        let config = DatabaseConfig {
            host: String::new(),
            ..DatabaseConfig::default()
        };
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(build_pool(&config))
            .unwrap_err();
        assert!(err.to_string().contains("database.host"));
    }
}
