//! The resolved-theme cache: Redis (24 h TTL by default) with a
//! process-local fallback for when Redis is not configured or unreachable.
//!
//! Publish/rollback events invalidate immediately — the TTL is only the
//! staleness bound when an invalidation event is lost. Key shape is
//! `sanvi:{tenant_id}:theming:resolved:{locale}` (the shared
//! `tenant_cache_key` contract).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use redis::AsyncCommands;
use uuid::Uuid;

use crate::domain::resolved::ResolvedTheme;

#[derive(Clone)]
struct LocalEntry {
    theme: ResolvedTheme,
    stored_at: Instant,
}

/// Local entries expire on read after this long (matches the default Redis
/// TTL; invalidation events clear them far sooner in practice).
const LOCAL_TTL: std::time::Duration = std::time::Duration::from_secs(86_400);

#[derive(Default)]
struct LocalInner {
    entries: HashMap<String, LocalEntry>,
}

/// Cheap to clone; the local state is shared.
#[derive(Clone)]
pub struct ResolvedThemeCache {
    redis: Option<redis::aio::ConnectionManager>,
    local: Arc<RwLock<LocalInner>>,
}

impl ResolvedThemeCache {
    pub fn new(redis: Option<redis::aio::ConnectionManager>) -> Self {
        Self {
            redis,
            local: Arc::new(RwLock::new(LocalInner::default())),
        }
    }

    fn key(tenant_id: Uuid, locale: &str) -> String {
        sanvi_cache::tenant_cache_key(tenant_id, "theming", &format!("resolved:{locale}"))
    }

    pub async fn get(&self, tenant_id: Uuid, locale: &str) -> Option<ResolvedTheme> {
        let key = Self::key(tenant_id, locale);
        if let Some(theme) = self.get_local(&key) {
            return Some(theme);
        }
        if let Some(manager) = &self.redis {
            let mut conn = manager.clone();
            match conn.get::<_, Option<String>>(&key).await {
                Ok(Some(json)) => match serde_json::from_str::<ResolvedTheme>(&json) {
                    Ok(theme) => {
                        self.set_local(key.clone(), theme.clone());
                        return Some(theme);
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "cached resolved theme failed to deserialize; evicting");
                        let _: Result<(), _> = conn.del(&key).await;
                    }
                },
                Ok(None) => {}
                Err(err) => {
                    // Redis trouble degrades to a miss (never an error).
                    tracing::debug!(error = %err, "resolved-theme cache read failed; treating as miss");
                }
            }
        }
        None
    }

    pub async fn set(&self, tenant_id: Uuid, locale: &str, theme: &ResolvedTheme, ttl_secs: u64) {
        let key = Self::key(tenant_id, locale);
        self.set_local(key.clone(), theme.clone());
        if let Some(manager) = &self.redis {
            let mut conn = manager.clone();
            let Ok(json) = serde_json::to_string(theme) else {
                return;
            };
            let result: Result<(), _> = conn.set_ex(&key, json, ttl_secs).await;
            if let Err(err) = result {
                tracing::debug!(error = %err, "resolved-theme cache write failed");
            }
        }
    }

    /// Drop every cached entry for one tenant (Redis + local). Called by
    /// the worker's event handler so every instance forgets the old theme.
    pub async fn invalidate_tenant(&self, tenant_id: Uuid) -> u64 {
        let mut deleted = 0u64;
        self.invalidate_local(tenant_id);
        if let Some(manager) = &self.redis {
            let pattern = sanvi_cache::invalidation_pattern(tenant_id);
            let mut scan_conn = manager.clone();
            let mut conn = manager.clone();
            match scan_conn.scan_match::<_, String>(&pattern).await {
                Ok(iter) => {
                    let mut iter = iter;
                    while let Some(key) = iter.next_item().await {
                        let key = match key {
                            Ok(key) => key,
                            Err(err) => {
                                tracing::warn!(error = %err, "failed while scanning tenant cache keys");
                                break;
                            }
                        };
                        match conn.del::<_, ()>(&key).await {
                            Ok(()) => deleted += 1,
                            Err(err) => {
                                tracing::warn!(error = %err, "failed to delete cached theme key");
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to scan tenant cache keys");
                }
            }
        }
        deleted
    }

    /// The synchronous local invalidation (used by the publish path before
    /// its transaction commits; the cross-instance invalidation rides the
    /// outbox event).
    pub fn invalidate_local(&self, tenant_id: Uuid) {
        let prefix = format!("sanvi:{tenant_id}:theming:");
        let mut guard = self.local.write().expect("cache lock is not poisoned");
        guard.entries.retain(|key, _| !key.starts_with(&prefix));
    }

    fn get_local(&self, key: &str) -> Option<ResolvedTheme> {
        let guard = self.local.read().expect("cache lock is not poisoned");
        let entry = guard.entries.get(key)?;
        if entry.stored_at.elapsed() > LOCAL_TTL {
            return None;
        }
        Some(entry.theme.clone())
    }

    fn set_local(&self, key: String, theme: ResolvedTheme) {
        let mut guard = self.local.write().expect("cache lock is not poisoned");
        guard.entries.insert(
            key,
            LocalEntry {
                theme,
                stored_at: Instant::now(),
            },
        );
    }

    /// The number of locally cached entries (tests/observability).
    pub fn local_len(&self) -> usize {
        self.local
            .read()
            .expect("cache lock is not poisoned")
            .entries
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::resolved::resolution_etag;

    fn theme(seed: i32) -> ResolvedTheme {
        ResolvedTheme {
            theme_key: "base".to_string(),
            theme_version: "1.0.0".to_string(),
            theme_api: "1.0.0".to_string(),
            capabilities: vec![],
            tokens: serde_json::json!({}),
            dark_tokens: None,
            css_vars: format!(":root{{--seed:{seed};}}"),
            layouts: Default::default(),
            fonts: vec![],
            theme_assets: Default::default(),
            brand_assets: Default::default(),
            custom_css: None,
            revision: seed,
            locale: "en".to_string(),
            etag: resolution_etag("base", "1.0.0", seed, "en"),
        }
    }

    #[tokio::test]
    async fn local_cache_round_trips_and_invalidates() {
        let cache = ResolvedThemeCache::new(None);
        let tenant = Uuid::now_v7();
        cache.set(tenant, "en", &theme(1), 60).await;
        assert_eq!(cache.get(tenant, "en").await.unwrap().revision, 1);
        cache.invalidate_local(tenant);
        assert!(cache.get(tenant, "en").await.is_none());
    }

    #[tokio::test]
    async fn local_invalidation_is_tenant_scoped() {
        let cache = ResolvedThemeCache::new(None);
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        cache.set(a, "en", &theme(1), 60).await;
        cache.set(b, "en", &theme(2), 60).await;
        cache.invalidate_local(a);
        assert!(cache.get(a, "en").await.is_none());
        assert_eq!(cache.get(b, "en").await.unwrap().revision, 2);
    }
}
