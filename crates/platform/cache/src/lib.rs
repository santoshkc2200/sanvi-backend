//! Sanvi cache: tenant-aware short-TTL caching.
//!
//! [`TenantCache`] enforces the key shape for tenant-scoped data that uses
//! it — every key is namespaced `sanvi:{tenant_id}:{context}:{key}` by
//! construction, so omitting the tenant from a cache key is impossible
//! rather than a code-review habit. Contexts may also own their own Redis
//! adapters with hand-rolled keys; what keeps those honest is the tenant-key
//! audit gate (`tests/key_audit.rs` in this crate), which every `src/` key
//! literal must pass: a tenant component in the format string or a
//! justified, reviewed exemption.
//!
//! Expensive shared keys should read through [`SingleFlight::get_or_load`]
//! (as [`TenantCache::get_or_load`] does): one loader per key per replica
//! per expiry, waiters re-read the cache, and hit ratios land on the
//! [`CacheMetrics`] instruments.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use redis::AsyncCommands;
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

pub mod metrics;
pub mod single_flight;

pub use metrics::CacheMetrics;
pub use single_flight::{FlightGuard, SingleFlight};

/// Tenant-namespaced cache over Redis.
#[derive(Clone)]
pub struct TenantCache {
    conn: redis::aio::ConnectionManager,
    single_flight: Arc<SingleFlight>,
    metrics: Option<CacheMetrics>,
}

impl TenantCache {
    pub fn new(conn: redis::aio::ConnectionManager) -> Self {
        Self {
            conn,
            single_flight: Arc::new(SingleFlight::new()),
            metrics: None,
        }
    }

    /// A cache that counts its hit ratio (TASK-021 11.2g).
    pub fn with_metrics(conn: redis::aio::ConnectionManager, metrics: CacheMetrics) -> Self {
        Self {
            conn,
            single_flight: Arc::new(SingleFlight::new()),
            metrics: Some(metrics),
        }
    }

    /// The stampede-protected read-through: at most one loader per key per
    /// replica runs; concurrent misses wait and re-read. The loader borrows
    /// the caller's environment for `'a` (the boxed-future shape
    /// [`SingleFlight::get_or_load`] documents).
    pub async fn get_or_load<'a, T, E>(
        &'a self,
        tenant_id: Uuid,
        context: &'a str,
        key: &'a str,
        ttl: Duration,
        load: impl Fn()
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, E>> + Send + 'a>>
        + Send
        + Sync
        + 'a,
    ) -> Result<T, E>
    where
        T: Serialize + DeserializeOwned + Clone + Send + Sync + 'a,
        E: Send + 'a,
    {
        let conn = self.conn.clone();
        let context_label = context.to_string();
        let full_key = self.key(tenant_id, context, key);
        let metrics = self.metrics.clone();
        let flight_key = format!("tenant:{full_key}");
        let conn_for_set = self.conn.clone();
        self.single_flight
            .get_or_load(
                &flight_key,
                || {
                    let conn = conn.clone();
                    let metrics = metrics.clone();
                    let context_label = context_label.clone();
                    let full_key = full_key.clone();
                    Box::pin(async move {
                        // Redis trouble degrades to a miss, never an error:
                        // the loader below re-reads through the same path.
                        match Self::get_on(&conn, tenant_id, context, key).await {
                            Ok(Some(hit)) => {
                                if let Some(metrics) = &metrics {
                                    metrics.hit(&context_label);
                                }
                                Some(hit)
                            }
                            Ok(None) => None,
                            Err(err) => {
                                tracing::warn!(error = %err, key = %full_key, "cache read failed; treating as miss");
                                None
                            }
                        }
                    })
                },
                move |value| {
                    let conn = conn_for_set.clone();
                    Box::pin(async move {
                        let _ = Self::set_on(&conn, tenant_id, context, key, &value, ttl).await;
                    })
                },
                || {
                    let metrics = metrics.clone();
                    let context_label = context_label.clone();
                    let inner = load();
                    Box::pin(async move {
                        if let Some(metrics) = &metrics {
                            metrics.miss(&context_label);
                        }
                        inner.await
                    })
                },
            )
            .await
    }

    /// The enforced key shape: `sanvi:{tenant_id}:{context}:{key}`.
    pub fn key(&self, tenant_id: Uuid, context: &str, key: &str) -> String {
        tenant_cache_key(tenant_id, context, key)
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        tenant_id: Uuid,
        context: &str,
        key: &str,
    ) -> Result<Option<T>> {
        Self::get_on(&self.conn, tenant_id, context, key).await
    }

    async fn get_on<T: DeserializeOwned>(
        conn: &redis::aio::ConnectionManager,
        tenant_id: Uuid,
        context: &str,
        key: &str,
    ) -> Result<Option<T>> {
        let full_key = tenant_cache_key(tenant_id, context, key);
        let mut conn = conn.clone();
        let raw: Option<String> = conn
            .get(&full_key)
            .await
            .with_context(|| format!("failed to read cache key {full_key}"))?;
        match raw {
            None => Ok(None),
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .with_context(|| format!("failed to deserialize cache value for {full_key}")),
        }
    }

    pub async fn set<T: Serialize>(
        &self,
        tenant_id: Uuid,
        context: &str,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<()> {
        Self::set_on(&self.conn, tenant_id, context, key, value, ttl).await
    }

    async fn set_on<T: Serialize>(
        conn: &redis::aio::ConnectionManager,
        tenant_id: Uuid,
        context: &str,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<()> {
        let full_key = tenant_cache_key(tenant_id, context, key);
        let json = serde_json::to_string(value).context("failed to serialize cache value")?;
        let mut conn = conn.clone();
        let _: () = conn
            .set_ex(&full_key, json, ttl.as_secs())
            .await
            .with_context(|| format!("failed to write cache key {full_key}"))?;
        Ok(())
    }

    pub async fn delete(&self, tenant_id: Uuid, context: &str, key: &str) -> Result<()> {
        let full_key = self.key(tenant_id, context, key);
        let mut conn = self.conn.clone();
        let _: () = conn
            .del(&full_key)
            .await
            .with_context(|| format!("failed to delete cache key {full_key}"))?;
        Ok(())
    }

    /// Drop every cached entry belonging to one tenant (event-driven
    /// invalidation on lifecycle/settings changes).
    pub async fn invalidate_tenant(&self, tenant_id: Uuid) -> Result<u64> {
        let pattern = invalidation_pattern(tenant_id);
        let mut scan_conn = self.conn.clone();
        let mut conn = self.conn.clone();
        let mut deleted = 0u64;
        let mut iter: redis::AsyncIter<String> = scan_conn
            .scan_match(&pattern)
            .await
            .with_context(|| format!("failed to scan cache for tenant {tenant_id}"))?;
        while let Some(key) = iter.next_item().await {
            let key = key.with_context(|| "failed while scanning tenant cache keys")?;
            let _: () = conn.del(&key).await?;
            deleted += 1;
        }
        Ok(deleted)
    }

    pub fn connection_manager(&self) -> redis::aio::ConnectionManager {
        self.conn.clone()
    }
}

/// The canonical tenant cache key (kept free of any connection for tests).
pub fn tenant_cache_key(tenant_id: Uuid, context: &str, key: &str) -> String {
    format!("sanvi:{tenant_id}:{context}:{key}")
}

/// Scan pattern covering every key of one tenant.
pub fn invalidation_pattern(tenant_id: Uuid) -> String {
    format!("sanvi:{tenant_id}:*")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape_is_enforced_by_construction() {
        let tenant = Uuid::nil();
        assert_eq!(
            tenant_cache_key(tenant, "settings", "theme"),
            format!("sanvi:{tenant}:settings:theme")
        );
    }

    #[test]
    fn invalidate_pattern_covers_the_whole_tenant() {
        let tenant = Uuid::now_v7();
        assert!(invalidation_pattern(tenant).starts_with(&format!("sanvi:{tenant}:")));
        assert!(invalidation_pattern(tenant).ends_with(":*"));
    }
}
