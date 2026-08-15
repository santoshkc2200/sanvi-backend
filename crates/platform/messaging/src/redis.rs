use anyhow::{Context, Result};
use redis::{AsyncCommands, aio::ConnectionManager};

use crate::RedisConfig;

/// Connect with the shared, reconnecting connection manager.
pub async fn connect_redis(config: &RedisConfig) -> Result<ConnectionManager> {
    config.validate()?;
    let client = redis::Client::open(config.url.as_str()).context("invalid redis URL")?;
    let manager = ConnectionManager::new(client)
        .await
        .context("failed to connect to redis")?;
    Ok(manager)
}

/// Minimal Redis Streams facade used by the dispatcher and (later) consumers.
#[derive(Clone)]
pub struct StreamsClient {
    conn: ConnectionManager,
}

impl StreamsClient {
    pub fn new(conn: ConnectionManager) -> Self {
        Self { conn }
    }

    /// Append a message to a stream, trimming to keep at most `max_len`
    /// entries (approximate trim).
    pub async fn append(
        &self,
        stream: &str,
        fields: &[(&str, &str)],
        max_len: usize,
    ) -> Result<String> {
        let mut conn = self.conn.clone();
        let options = redis::streams::StreamAddOptions::default().trim(
            redis::streams::StreamTrimStrategy::maxlen(
                redis::streams::StreamTrimmingMode::Approx,
                max_len,
            ),
        );
        let id: String = conn
            .xadd_options(stream, "*", fields, &options)
            .await
            .with_context(|| format!("failed to append to stream {stream}"))?;
        Ok(id)
    }

    /// Create a consumer group at the tail of the stream (idempotent).
    pub async fn ensure_consumer_group(&self, stream: &str, group: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        match redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(stream)
            .arg(group)
            .arg("$")
            .arg("MKSTREAM")
            .query_async::<()>(&mut conn)
            .await
        {
            Ok(()) => Ok(()),
            // "BUSYGROUP" means the group already exists: fine.
            Err(err) if err.to_string().contains("BUSYGROUP") => Ok(()),
            Err(err) => Err(err)
                .with_context(|| format!("failed to create consumer group {group} on {stream}")),
        }
    }

    /// Range-read a stream (used by tests and tooling).
    pub async fn range(
        &self,
        stream: &str,
        start: &str,
        end: &str,
    ) -> Result<Vec<(String, Vec<(String, String)>)>> {
        let mut conn = self.conn.clone();
        let entries: Vec<(String, Vec<(String, String)>)> =
            conn.xrange(stream, start, end)
                .await
                .with_context(|| format!("failed to range-read stream {stream}"))?;
        Ok(entries)
    }

    pub fn connection_manager(&self) -> ConnectionManager {
        self.conn.clone()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn config_validation_rejects_bad_urls() {
        assert!(
            crate::RedisConfig {
                url: "not a url".to_string()
            }
            .validate()
            .is_err()
        );
    }
}
