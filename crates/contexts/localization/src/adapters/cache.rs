//! In-memory caches for everything on the localization hot path: merged
//! catalogs (static + overrides), tenant locale settings, the platform
//! locale set and user locale preferences.
//!
//! Cross-instance coherence comes from TTLs (all short) plus event-driven
//! invalidation on the instance that made the change; the worker consumes
//! the same events and invalidates its own copy.

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use uuid::Uuid;

use crate::adapters::catalog::MergedCatalog;
use crate::domain::{LocaleSet, TenantLocaleSettings};

#[derive(Clone)]
enum CacheValue {
    Merged(Arc<MergedCatalog>),
    TenantSettings(Option<TenantLocaleSettings>),
    PlatformSet(Arc<LocaleSet>),
    UserLocale(Option<String>),
}

struct CacheEntry {
    value: CacheValue,
    stored_at: Instant,
}

#[derive(Default)]
struct CacheInner {
    entries: HashMap<String, CacheEntry>,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// Cheap to clone; the inner state is shared.
#[derive(Clone)]
pub struct LocalizationCache {
    inner: Arc<RwLock<CacheInner>>,
}

impl Default for LocalizationCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalizationCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(CacheInner::default())),
        }
    }

    fn get(&self, key: &str, ttl: Duration) -> Option<CacheValue> {
        let guard = self.inner.read().expect("cache lock is not poisoned");
        let entry = guard.entries.get(key)?;
        if entry.stored_at.elapsed() > ttl {
            return None;
        }
        entry.value.clone().into()
    }

    fn set(&self, key: impl Into<String>, value: CacheValue) {
        let mut guard = self.inner.write().expect("cache lock is not poisoned");
        guard.entries.insert(
            key.into(),
            CacheEntry {
                value,
                stored_at: Instant::now(),
            },
        );
    }

    pub fn get_merged(
        &self,
        scope: &str,
        locale: &str,
        ttl: Duration,
    ) -> Option<Arc<MergedCatalog>> {
        match self.get(&merged_key(scope, locale), ttl) {
            Some(CacheValue::Merged(merged)) => {
                self.inner
                    .read()
                    .expect("cache lock is not poisoned")
                    .hits
                    .fetch_add(1, Ordering::Relaxed);
                Some(merged)
            }
            _ => None,
        }
    }

    pub fn set_merged(&self, scope: &str, locale: &str, merged: Arc<MergedCatalog>) {
        self.set(merged_key(scope, locale), CacheValue::Merged(merged));
    }

    pub fn get_tenant_settings(
        &self,
        tenant_id: Uuid,
        ttl: Duration,
    ) -> Option<Option<TenantLocaleSettings>> {
        match self.get(&tenant_settings_key(tenant_id), ttl) {
            Some(CacheValue::TenantSettings(settings)) => Some(settings),
            _ => None,
        }
    }

    pub fn set_tenant_settings(&self, tenant_id: Uuid, settings: Option<TenantLocaleSettings>) {
        self.set(
            tenant_settings_key(tenant_id),
            CacheValue::TenantSettings(settings),
        );
    }

    pub fn get_platform_set(&self, ttl: Duration) -> Option<Arc<LocaleSet>> {
        match self.get("platform_set", ttl) {
            Some(CacheValue::PlatformSet(set)) => Some(set),
            _ => None,
        }
    }

    pub fn set_platform_set(&self, set: Arc<LocaleSet>) {
        self.set("platform_set", CacheValue::PlatformSet(set));
    }

    pub fn get_user_locale(&self, user_id: Uuid, ttl: Duration) -> Option<Option<String>> {
        match self.get(&user_locale_key(user_id), ttl) {
            Some(CacheValue::UserLocale(locale)) => Some(locale),
            _ => None,
        }
    }

    pub fn set_user_locale(&self, user_id: Uuid, locale: Option<String>) {
        self.set(user_locale_key(user_id), CacheValue::UserLocale(locale));
    }

    /// Record a miss for the hit-ratio metric (called by callers on the
    /// paths that check `get_*` and decide to load).
    pub fn record_miss(&self) {
        self.inner
            .read()
            .expect("cache lock is not poisoned")
            .misses
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Invalidate every merged catalog for one scope (tenant id or
    /// `"platform"`).
    pub fn invalidate_scope(&self, scope: &str) {
        let mut guard = self.inner.write().expect("cache lock is not poisoned");
        let prefix = format!("merged:{scope}:");
        guard.entries.retain(|key, _| !key.starts_with(&prefix));
    }

    /// Platform copy is embedded in every scope's merged catalog.
    pub fn invalidate_all_merged(&self) {
        let mut guard = self.inner.write().expect("cache lock is not poisoned");
        guard.entries.retain(|key, _| !key.starts_with("merged:"));
    }

    /// Invalidate one tenant's locale settings (and its merged catalogs,
    /// which embed its overrides and default locale).
    pub fn invalidate_tenant(&self, tenant_id: Uuid) {
        let mut guard = self.inner.write().expect("cache lock is not poisoned");
        guard
            .entries
            .retain(|key, _| key != &tenant_settings_key(tenant_id));
        let prefix = format!("merged:{}:", tenant_id);
        guard.entries.retain(|key, _| !key.starts_with(&prefix));
    }

    /// Invalidate a user's cached locale preference.
    pub fn invalidate_user(&self, user_id: Uuid) {
        let mut guard = self.inner.write().expect("cache lock is not poisoned");
        guard.entries.remove(&user_locale_key(user_id));
    }

    pub fn clear(&self) {
        let mut guard = self.inner.write().expect("cache lock is not poisoned");
        guard.entries.clear();
    }

    pub fn hit_ratio(&self) -> f64 {
        let guard = self.inner.read().expect("cache lock is not poisoned");
        let hits = guard.hits.load(Ordering::Relaxed) as f64;
        let misses = guard.misses.load(Ordering::Relaxed) as f64;
        if hits + misses == 0.0 {
            1.0
        } else {
            hits / (hits + misses)
        }
    }
}

fn merged_key(scope: &str, locale: &str) -> String {
    format!("merged:{scope}:{locale}")
}

fn tenant_settings_key(tenant_id: Uuid) -> String {
    format!("tenant_settings:{tenant_id}")
}

fn user_locale_key(user_id: Uuid) -> String {
    format!("user_locale:{user_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_expire_after_their_ttl() {
        let cache = LocalizationCache::new();
        cache.set_merged("platform", "en", Arc::new(MergedCatalog::default()));
        assert!(
            cache
                .get_merged("platform", "en", Duration::from_secs(5))
                .is_some()
        );
        assert!(
            cache
                .get_merged("platform", "en", Duration::from_secs(0))
                .is_none()
        );
    }

    #[test]
    fn invalidation_is_scoped() {
        let cache = LocalizationCache::new();
        let merged = Arc::new(MergedCatalog::default());
        cache.set_merged("platform", "en", merged.clone());
        cache.set_merged("11111111-1111-1111-1111-111111111111", "en", merged);
        cache.invalidate_scope("platform");
        assert!(
            cache
                .get_merged("platform", "en", Duration::from_secs(60))
                .is_none()
        );
        assert!(
            cache
                .get_merged(
                    "11111111-1111-1111-1111-111111111111",
                    "en",
                    Duration::from_secs(60)
                )
                .is_some()
        );
    }

    #[test]
    fn hit_ratio_counts() {
        let cache = LocalizationCache::new();
        assert_eq!(cache.hit_ratio(), 1.0);
        cache.record_miss();
        assert_eq!(cache.hit_ratio(), 0.0);
        cache.set_merged("platform", "en", Arc::new(MergedCatalog::default()));
        cache.get_merged("platform", "en", Duration::from_secs(60));
        assert_eq!(cache.hit_ratio(), 0.5);
    }
}
