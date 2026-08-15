use anyhow::{Context, Result};
use redis::{
    AsyncCommands,
    aio::{ConnectionManager, ConnectionManagerConfig},
};

use crate::RedisConfig;

/// Connect with the shared, reconnecting connection manager.
///
/// Timeouts are taken from config rather than the library defaults, so a
/// bump of the `redis` crate can't silently change how fast we give up on a
/// slow Redis (`ConnectionManager::new()` alone defaults to 1s connect /
/// 500ms response as of redis 1.x).
pub async fn connect_redis(config: &RedisConfig) -> Result<ConnectionManager> {
    config.validate()?;
    let client = redis::Client::open(config.url.as_str()).context("invalid redis URL")?;
    let manager_config = ConnectionManagerConfig::new()
        .set_connection_timeout(Some(config.connect_timeout()))
        .set_response_timeout(Some(config.response_timeout()));
    let manager = ConnectionManager::new_with_config(client, manager_config)
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

    /// Create a consumer group at the start of the stream (idempotent).
    ///
    /// `0` (not `$`) on purpose: a brand-new consumer group must see events
    /// that predate it — handlers are idempotent under the processed-events
    /// dedup key, so replaying the stream on first start is safe, while a
    /// `$` group silently loses everything published before it existed.
    pub async fn ensure_consumer_group(&self, stream: &str, group: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        match redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(stream)
            .arg(group)
            .arg("0")
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
                url: "not a url".to_string(),
                ..crate::RedisConfig::default()
            }
            .validate()
            .is_err()
        );
    }
}
