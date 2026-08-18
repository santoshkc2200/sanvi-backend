//! The host → tenant cache: Redis with an in-process fallback.
//!
//! Positive and negative entries are cached; the worker's domains consumer
//! invalidates across instances on verification/removal events, and the
//! API invalidates its local copy in-process after state changes.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::domain::DomainsError;

pub struct LiveDomainCache {
    redis: Option<redis::aio::ConnectionManager>,
    local: Mutex<HashMap<String, Option<uuid::Uuid>>>,
    ttl_secs: u64,
}

impl LiveDomainCache {
    pub fn new(redis: Option<redis::aio::ConnectionManager>, ttl_secs: u64) -> Self {
        Self {
            redis,
            local: Mutex::new(HashMap::new()),
            ttl_secs,
        }
    }

    pub async fn get(&self, normalized_hostname: &str) -> Option<uuid::Uuid> {
        if let Some(tenant) = self
            .local
            .lock()
            .await
            .get(normalized_hostname)
            .copied()
            .flatten()
        {
            return Some(tenant);
        }
        if let Some(redis) = &self.redis {
            let key = cache_key(normalized_hostname);
            let mut connection = redis.clone();
            let raw: Option<String> = redis::cmd("GET")
                .arg(&key)
                .query_async::<Option<String>>(&mut connection)
                .await
                .ok()
                .flatten();
            if let Some(raw) = raw {
                let tenant = uuid::Uuid::parse_str(&raw).ok();
                self.local
                    .lock()
                    .await
                    .insert(normalized_hostname.to_string(), tenant);
                return tenant;
            }
        }
        None
    }

    pub async fn put(&self, normalized_hostname: String, tenant: Option<uuid::Uuid>) {
        self.local
            .lock()
            .await
            .insert(normalized_hostname.clone(), tenant);
        if let Some(redis) = &self.redis {
            let key = cache_key(&normalized_hostname);
            let value = tenant.map(|tenant| tenant.to_string()).unwrap_or_default();
            let mut connection = redis.clone();
            let _: Result<(), redis::RedisError> = redis::cmd("SET")
                .arg(&key)
                .arg(&value)
                .arg("EX")
                .arg(self.ttl_secs)
                .query_async(&mut connection)
                .await;
        }
    }

    /// Drop both layers (called by the worker consumer and in-process).
    pub async fn invalidate(&self, normalized_hostname: &str) -> Result<(), DomainsError> {
        self.local.lock().await.remove(normalized_hostname);
        if let Some(redis) = &self.redis {
            let key = cache_key(normalized_hostname);
            let mut connection = redis.clone();
            redis::cmd("DEL")
                .arg(&key)
                .query_async::<i64>(&mut connection)
                .await
                .map_err(|err| DomainsError::Cache(err.to_string()))?;
        }
        Ok(())
    }
}

pub type SharedLiveDomainCache = Arc<LiveDomainCache>;

fn cache_key(normalized_hostname: &str) -> String {
    format!("sanvi:domains:host:{normalized_hostname}")
}
