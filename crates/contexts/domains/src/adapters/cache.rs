//! The host → tenant cache: Redis with an in-process fallback.
//!
//! Positive and negative entries are cached in Redis under the shared TTL;
//! the worker's domains consumer invalidates across instances on
//! verification/removal events, and the API invalidates its local copy
//! in-process after state changes. The local layer caches only positive
//! entries (a miss must always re-check: resolution can flip to positive
//! on another instance), with the same TTL as Redis and a size cap —
//! `resolve` runs per request on attacker-influenced Host headers, so an
//! unbounded, never-expiring local map is both a leak and a stale-answer
//! hazard (nothing cascades a tenant deletion into a cache invalidation).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use sanvi_kernel::TenantId;

use crate::domain::DomainsError;

/// Upper bound for the in-process positive cache. When reached, expired
/// entries are pruned; if it is still full the insert is dropped (Redis
/// keeps serving the hot path).
const LOCAL_CACHE_CAP: usize = 8192;

pub struct LiveDomainCache {
    redis: Option<redis::aio::ConnectionManager>,
    local: Mutex<HashMap<String, (TenantId, tokio::time::Instant)>>,
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

    pub async fn get(&self, normalized_hostname: &str) -> Option<TenantId> {
        {
            let mut local = self.local.lock().await;
            match local.get(normalized_hostname) {
                Some((tenant, at)) if at.elapsed().as_secs() < self.ttl_secs => {
                    return Some(*tenant);
                }
                Some(_) => {
                    local.remove(normalized_hostname);
                }
                None => {}
            }
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
                if let Ok(tenant) = raw.parse::<TenantId>() {
                    self.remember_local(normalized_hostname.to_string(), tenant)
                        .await;
                    return Some(tenant);
                }
                return None;
            }
        }
        None
    }

    pub async fn put(&self, normalized_hostname: String, tenant: Option<TenantId>) {
        if let Some(tenant) = tenant {
            self.remember_local(normalized_hostname.clone(), tenant)
                .await;
        }
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

    async fn remember_local(&self, normalized_hostname: String, tenant: TenantId) {
        let mut local = self.local.lock().await;
        if local.len() >= LOCAL_CACHE_CAP {
            local.retain(|_, (_, at)| at.elapsed().as_secs() < self.ttl_secs);
        }
        if local.len() < LOCAL_CACHE_CAP {
            local.insert(normalized_hostname, (tenant, tokio::time::Instant::now()));
        }
    }
}

pub type SharedLiveDomainCache = Arc<LiveDomainCache>;

fn cache_key(normalized_hostname: &str) -> String {
    format!("sanvi:domains:host:{normalized_hostname}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn positive_entries_hit_and_expire() {
        let cache = LiveDomainCache::new(None, 3600);
        let tenant = TenantId::try_from(1i64).unwrap();
        cache
            .put("shop.example.com".to_string(), Some(tenant))
            .await;
        assert_eq!(cache.get("shop.example.com").await, Some(tenant));

        // TTL 0: every entry is immediately stale, so reads fall through.
        let expiring = LiveDomainCache::new(None, 0);
        expiring
            .put("shop.example.com".to_string(), Some(TenantId::try_from(2i64).unwrap()))
            .await;
        assert!(expiring.get("shop.example.com").await.is_none());
        assert!(expiring.local.lock().await.is_empty());
    }

    #[tokio::test]
    async fn negative_entries_never_enter_the_local_layer() {
        let cache = LiveDomainCache::new(None, 3600);
        cache.put("unknown.example.com".to_string(), None).await;
        assert!(cache.local.lock().await.is_empty());
    }
}
