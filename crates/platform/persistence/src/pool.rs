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
        "connected to postgres",
    );

    Ok(pool)
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
