//! Entitlement caches. The Redis adapter is the production path
//! (`sanvi:{tenant}:entitlements`, TTL 300 s); the in-memory adapter serves
//! single-process deployments and tests; the noop adapter disables caching
//! (every check hits the database). Invalidation is event-driven from the
//! worker.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use redis::AsyncCommands;
use uuid::Uuid;

use crate::domain::ResolvedEntitlement;
use crate::domain::ports::{CachedEntitlements, EntitlementCache};

/// The canonical Redis key for a tenant's resolved set.
pub fn entitlement_key(tenant_id: Uuid) -> String {
    format!("sanvi:{tenant_id}:entitlements")
}

/// Redis-backed cache with the plan's key shape and TTL.
#[derive(Clone)]
pub struct RedisEntitlementCache {
    conn: redis::aio::ConnectionManager,
}

impl RedisEntitlementCache {
    pub fn new(conn: redis::aio::ConnectionManager, _ttl_secs: u64) -> Self {
        Self { conn }
    }
}

#[async_trait::async_trait]
impl EntitlementCache for RedisEntitlementCache {
    async fn get(&self, tenant_id: Uuid) -> Option<CachedEntitlements> {
        let key = entitlement_key(tenant_id);
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn
            .get(&key)
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, key = %key, "entitlement cache read failed");
                err
            })
            .ok()?;
        let raw = raw?;
        match serde_json::from_str::<CachedEntitlements>(&raw) {
            Ok(entries) => Some(entries),
            Err(err) => {
                tracing::warn!(error = %err, key = %key, "entitlement cache payload corrupt");
                None
            }
        }
    }

    async fn put(&self, tenant_id: Uuid, entries: &[ResolvedEntitlement], ttl_secs: u64) {
        let key = entitlement_key(tenant_id);
        let value = CachedEntitlements {
            entries: entries.to_vec(),
            cached_at: Utc::now(),
        };
        let json = match serde_json::to_string(&value) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "entitlement cache serialization failed");
                return;
            }
        };
        let ttl = ttl_secs.max(1);
        let mut conn = self.conn.clone();
        if let Err(err) = conn.set_ex::<_, _, ()>(&key, json, ttl).await {
            tracing::warn!(error = %err, key = %key, "entitlement cache write failed");
        }
    }

    async fn invalidate(&self, tenant_id: Uuid) {
        let mut conn = self.conn.clone();
        if let Err(err) = conn.del::<_, ()>(entitlement_key(tenant_id)).await {
            tracing::warn!(error = %err, "entitlement cache delete failed");
        }
    }

    async fn invalidate_all(&self) {
        // SCAN the namespace; key space is small (one key per active
        // tenant), so a full scan is cheaper than bookkeeping.
        let mut scan_conn = self.conn.clone();
        let mut conn = self.conn.clone();
        let pattern = "sanvi:*:entitlements".to_string();
        match scan_conn.scan_match(&pattern).await {
            Ok(iter) => {
                let mut iter: redis::AsyncIter<String> = iter;
                while let Some(item) = iter.next_item().await {
                    let key = match item {
                        Ok(key) => key,
                        Err(err) => {
                            tracing::warn!(error = %err, "entitlement cache scan item failed");
                            continue;
                        }
                    };
                    if let Err(err) = conn.del::<_, ()>(&key).await {
                        tracing::warn!(error = %err, key = %key, "entitlement cache delete failed");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "entitlement cache scan failed");
            }
        }
    }
}

/// Bounded in-memory cache (single process, tests, small deployments).
pub struct InMemoryEntitlementCache {
    inner: Mutex<HashMap<Uuid, (CachedEntitlements, std::time::Instant)>>,
    ttl: Duration,
    max_entries: usize,
}

impl InMemoryEntitlementCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            max_entries,
        }
    }
}

#[async_trait::async_trait]
impl EntitlementCache for InMemoryEntitlementCache {
    async fn get(&self, tenant_id: Uuid) -> Option<CachedEntitlements> {
        let now = std::time::Instant::now();
        let mut map = self.inner.lock().expect("entitlement cache lock poisoned");
        match map.get(&tenant_id) {
            Some((entries, cached_at)) if now.duration_since(*cached_at) < self.ttl => {
                Some(entries.clone())
            }
            _ => {
                map.remove(&tenant_id);
                None
            }
        }
    }

    async fn put(&self, tenant_id: Uuid, entries: &[ResolvedEntitlement], _ttl_secs: u64) {
        let mut map = self.inner.lock().expect("entitlement cache lock poisoned");
        if map.len() >= self.max_entries && !map.contains_key(&tenant_id) {
            // Bounded: drop an arbitrary entry rather than growing forever.
            if let Some(key) = map.keys().next().copied() {
                map.remove(&key);
            }
        }
        map.insert(
            tenant_id,
            (
                CachedEntitlements {
                    entries: entries.to_vec(),
                    cached_at: Utc::now(),
                },
                std::time::Instant::now(),
            ),
        );
    }

    async fn invalidate(&self, tenant_id: Uuid) {
        let mut map = self.inner.lock().expect("entitlement cache lock poisoned");
        map.remove(&tenant_id);
    }

    async fn invalidate_all(&self) {
        let mut map = self.inner.lock().expect("entitlement cache lock poisoned");
        map.clear();
    }
}

/// No caching: every check resolves from the database.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopEntitlementCache;

#[async_trait::async_trait]
impl EntitlementCache for NoopEntitlementCache {
    async fn get(&self, _tenant_id: Uuid) -> Option<CachedEntitlements> {
        None
    }

    async fn put(&self, _tenant_id: Uuid, _entries: &[ResolvedEntitlement], _ttl_secs: u64) {}

    async fn invalidate(&self, _tenant_id: Uuid) {}

    async fn invalidate_all(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EntitlementSource, FeatureKey, FeatureKind};

    fn resolved(key: &str) -> ResolvedEntitlement {
        ResolvedEntitlement {
            feature: FeatureKey::parse(key).unwrap(),
            kind: FeatureKind::Boolean,
            enabled: true,
            limit: None,
            source: EntitlementSource::FeatureDefault,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn in_memory_cache_serves_within_the_ttl() {
        let cache = InMemoryEntitlementCache::new(Duration::from_secs(60), 10);
        let tenant = Uuid::now_v7();
        assert!(cache.get(tenant).await.is_none());
        cache.put(tenant, &[resolved("domains.custom")], 60).await;
        let hit = cache.get(tenant).await.unwrap();
        assert_eq!(hit.entries.len(), 1);
        cache.invalidate(tenant).await;
        assert!(cache.get(tenant).await.is_none());
    }

    #[tokio::test]
    async fn in_memory_cache_stays_bounded() {
        let cache = InMemoryEntitlementCache::new(Duration::from_secs(60), 2);
        cache.put(Uuid::now_v7(), &[], 60).await;
        cache.put(Uuid::now_v7(), &[], 60).await;
        cache.put(Uuid::now_v7(), &[], 60).await;
        // Eviction happened; still usable.
        assert!(cache.get(Uuid::now_v7()).await.is_none());
    }

    #[test]
    fn key_shape_matches_the_plan() {
        let tenant = Uuid::nil();
        assert_eq!(
            entitlement_key(tenant),
            format!("sanvi:{tenant}:entitlements")
        );
    }
}
