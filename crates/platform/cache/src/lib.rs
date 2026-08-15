//! Sanvi cache: tenant-aware short-TTL caching.
//!
//! [`TenantCache`] is the only way tenant-scoped data reaches Redis from
//! application code: every key is namespaced `sanvi:{tenant_id}:{context}:{key}`
//! by construction, so omitting the tenant from a cache key is impossible
//! rather than a code-review habit.

use std::time::Duration;

use anyhow::{Context, Result};
use redis::AsyncCommands;
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

/// Tenant-namespaced cache over Redis.
#[derive(Clone)]
pub struct TenantCache {
    conn: redis::aio::ConnectionManager,
}

impl TenantCache {
    pub fn new(conn: redis::aio::ConnectionManager) -> Self {
        Self { conn }
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
        let full_key = self.key(tenant_id, context, key);
        let mut conn = self.conn.clone();
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
        let full_key = self.key(tenant_id, context, key);
        let json = serde_json::to_string(value).context("failed to serialize cache value")?;
        let mut conn = self.conn.clone();
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
