//! The SSR cache key (TASK-022, FR-1111): the type that makes a wrong-tenant
//! response unrepresentable.
//!
//! Every dimension the storefront response depends on is a partition of the
//! key space: two tenants can never share an entry, and locale, theme
//! revision and device class each split it further. The tenant is a
//! constructor argument of type [`TenantId`] — there is no `Default`, no
//! setter and no `Deserialize` impl, so a tenant-less key fails to
//! construct at the type level, not by review.
//!
//! The canonical Redis key nests under the tenant-namespaced shape the
//! key-audit gate enforces: `sanvi:{tenant_id}:ssr:{…}` — which also makes
//! per-tenant invalidation a single `SCAN` prefix (`ssr_invalidation_pattern`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use prometheus::{IntCounter, Registry};
use redis::AsyncCommands;
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};

use crate::single_flight::SingleFlight;

/// The device partition of the key (FR-1111). Introduced because the
/// storefront serves different markup to mobile; if that ever stops being
/// true, remove the dimension rather than leave one that only multiplies
/// entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    Desktop,
    Mobile,
}

impl DeviceClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
        }
    }
}

/// A construction input carried characters the canonical key cannot hold.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid SSR cache key component `{name}`: {reason}")]
pub struct SsrKeyError {
    pub name: &'static str,
    pub reason: &'static str,
}

/// The SSR cache key: `(tenant, locale, theme revision, device class)` plus
/// the public route (path with its non-host query) the entry was rendered
/// for. Equality is exact per dimension — the partition tests in
/// `tests/ssr_cache_negative.rs` pin that two keys differing in any single
/// dimension never share an entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SsrCacheKey {
    tenant_id: TenantId,
    locale: String,
    theme_revision: i32,
    device_class: DeviceClass,
    route: String,
}

/// Locale labels are lowercased BCP-47-ish tags (`en`, `ja`, `pt-br`).
/// Anything outside this alphabet would smuggle `:` or whitespace into the
/// canonical key, so construction rejects it.
fn validate_locale(locale: &str) -> Result<(), SsrKeyError> {
    let ok = !locale.is_empty()
        && locale.len() <= 35
        && locale
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(SsrKeyError {
            name: "locale",
            reason: "expected a lowercase BCP-47 tag of at most 35 characters (a-z, 0-9, '-')",
        })
    }
}

/// The route part is the request path plus its query, minus the `host`
/// parameter (the host *is* the tenant dimension; keeping it would multiply
/// entries per tenant without partitioning anything). Percent-encoded wire
/// bytes are already safe; the length cap keeps a pathological query from
/// bloating Redis keys.
fn validate_route(route: &str) -> Result<(), SsrKeyError> {
    let ok = route.starts_with('/')
        && route.len() <= 512
        && !route.chars().any(|c| c.is_control() || c == ' ');
    if ok {
        Ok(())
    } else {
        Err(SsrKeyError {
            name: "route",
            reason: "expected a request path (+query) of at most 512 printable characters",
        })
    }
}

impl SsrCacheKey {
    /// The only constructor. A key without a tenant is unrepresentable:
    /// `TenantId` is taken by value and stored privately.
    pub fn new(
        tenant_id: TenantId,
        locale: impl Into<String>,
        theme_revision: i32,
        device_class: DeviceClass,
        route: impl Into<String>,
    ) -> Result<Self, SsrKeyError> {
        let locale = locale.into();
        let route = route.into();
        validate_locale(&locale)?;
        validate_route(&route)?;
        Ok(Self {
            tenant_id,
            locale,
            theme_revision,
            device_class,
            route,
        })
    }

    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }

    pub fn theme_revision(&self) -> i32 {
        self.theme_revision
    }

    pub fn device_class(&self) -> DeviceClass {
        self.device_class
    }

    pub fn route(&self) -> &str {
        &self.route
    }

    /// The canonical Redis key under the enforced tenant namespace. The
    /// tenant component is spelled out for the key-audit gate: a key that
    /// forgets it is a bypass of RLS.
    pub fn to_cache_key(&self) -> String {
        format!(
            "sanvi:{tenant_id}:ssr:{device}/{revision}/{locale}?{route}",
            tenant_id = self.tenant_id,
            device = self.device_class.as_str(),
            revision = self.theme_revision,
            locale = self.locale,
            route = percent_encode_route(&self.route),
        )
    }
}

/// Route segments can hold `/` and `?` by nature; only `%` needs escaping so
/// the key stays one unambiguous string.
fn percent_encode_route(route: &str) -> String {
    route.replace('%', "%25")
}

/// The SSR cache's own counters (TASK-022 11.3c/11.3g): a dedicated metric
/// family, because the shared `CacheMetrics` family (`cache_hits_total`) is
/// owned per process by whichever adapter registers first — the
/// entitlements cache — and a second registration of the same names is
/// rejected with the error swallowed, silently orphaning its increments.
/// The hit ratio is
/// `ssr_cache_hits_total / (ssr_cache_hits_total + ssr_cache_misses_total)`.
#[derive(Clone)]
pub struct SsrCacheMetrics {
    hits: IntCounter,
    misses: IntCounter,
    joins: IntCounter,
}

impl SsrCacheMetrics {
    pub fn new(registry: &Registry) -> Self {
        let build = |name: &str, help: &str| {
            let counter = IntCounter::new(name, help).expect("counter construction is infallible");
            let _ = registry.register(Box::new(counter.clone()));
            counter
        };
        Self {
            hits: build(
                "ssr_cache_hits_total",
                "Storefront reads served from the SSR cache",
            ),
            misses: build(
                "ssr_cache_misses_total",
                "Storefront reads that rendered (SSR cache misses)",
            ),
            joins: build(
                "ssr_cache_joins_total",
                "Concurrent misses that joined an in-flight render",
            ),
        }
    }

    /// Counting without a registry (tests, single-process deployments).
    pub fn noop() -> Self {
        let build = |name: &str, help: &str| {
            IntCounter::new(name, help).expect("counter construction is infallible")
        };
        Self {
            hits: build("ssr_cache_hits_total", "hits"),
            misses: build("ssr_cache_misses_total", "misses"),
            joins: build("ssr_cache_joins_total", "joins"),
        }
    }

    pub fn hit(&self) {
        self.hits.inc();
    }

    pub fn miss(&self) {
        self.misses.inc();
    }

    pub fn join(&self) {
        self.joins.inc();
    }
}

/// A cached SSR response: the status, the headers worth replaying (every
/// header present at the cache layer, minus `content-length`, which the
/// response builder re-derives), and the identity-encoded body bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsrEntry {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Cap for the process-local fallback map (Redis-backed deployments never
/// touch it).
const DEFAULT_MAX_LOCAL_ENTRIES: usize = 4096;

struct LocalEntry {
    entry: SsrEntry,
    stored_at: Instant,
    ttl: Duration,
}

/// The bounded process-local fallback used only when Redis is not
/// configured (mirrors `ResolvedThemeCache`): correctness first — the local
/// map is shared by every worker of the process, capped, and TTL-checked.
#[derive(Default)]
struct LocalInner {
    entries: HashMap<String, LocalEntry>,
}

/// The SSR response cache (TASK-022, FR-1111): Redis-backed when Redis is
/// configured — the shared layer the worker's event-driven invalidation
/// acts on — with a process-local fallback otherwise. Cache trouble
/// degrades to a miss, never an error: the flag-off path is always
/// available.
#[derive(Clone)]
pub struct SsrCache {
    redis: Option<redis::aio::ConnectionManager>,
    local: Arc<RwLock<LocalInner>>,
    single_flight: SingleFlight,
    metrics: Option<SsrCacheMetrics>,
    max_local_entries: usize,
}

impl SsrCache {
    pub fn new(
        redis: Option<redis::aio::ConnectionManager>,
        metrics: Option<SsrCacheMetrics>,
    ) -> Self {
        Self {
            redis,
            local: Arc::new(RwLock::new(LocalInner::default())),
            single_flight: SingleFlight::new(),
            metrics,
            max_local_entries: DEFAULT_MAX_LOCAL_ENTRIES,
        }
    }

    /// The shared flight map: one render per key per process, whatever the
    /// backing store.
    pub fn single_flight(&self) -> &SingleFlight {
        &self.single_flight
    }

    /// Count a render: a read that fell through to a load.
    pub fn record_miss(&self) {
        if let Some(metrics) = &self.metrics {
            metrics.miss();
        }
    }

    /// Count a concurrent miss that joined an in-flight render.
    pub fn record_join(&self) {
        if let Some(metrics) = &self.metrics {
            metrics.join();
        }
    }

    /// Read one entry. `None` is a miss — including on any cache error,
    /// which only logs.
    pub async fn get(&self, key: &SsrCacheKey) -> Option<SsrEntry> {
        let canonical = key.to_cache_key();

        if let Some(manager) = &self.redis {
            let mut conn = manager.clone();
            match conn.get::<_, Option<String>>(&canonical).await {
                Ok(Some(json)) => match serde_json::from_str::<SsrEntry>(&json) {
                    Ok(entry) => {
                        if let Some(metrics) = &self.metrics {
                            metrics.hit();
                        }
                        return Some(entry);
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            key = %canonical,
                            "cached SSR entry failed to deserialize; evicting"
                        );
                        let _: Result<(), _> = conn.del(&canonical).await;
                    }
                },
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(
                        error = %err,
                        key = %canonical,
                        "SSR cache read failed; treating as miss"
                    );
                }
            }
            return None;
        }
        // Process-local fallback (Redis absent): shared across the
        // process's workers, bounded, TTL-checked. A local entry cannot be
        // invalidated by another process — the worker's invalidation path
        // clears it in the same process, and the TTL bounds anything left.
        let hit = {
            let mut inner = self.local.write().expect("ssr local cache lock poisoned");
            match inner.entries.get(&canonical) {
                // A read is a peek, never a removal: several waiters exit
                // through the same entry after one render.
                Some(local) if local.stored_at.elapsed() < local.ttl => Some(local.entry.clone()),
                Some(_) => {
                    // Expired: take it out (already dead).
                    inner.entries.remove(&canonical);
                    None
                }
                None => None,
            }
        };
        if hit.is_some()
            && let Some(metrics) = &self.metrics
        {
            metrics.hit();
        }
        hit
    }

    /// Write one entry. Best-effort: a failed write costs the next read a
    /// render, never a wrong response.
    pub async fn put(&self, key: &SsrCacheKey, entry: &SsrEntry, ttl: Duration) {
        let canonical = key.to_cache_key();
        if let Some(manager) = &self.redis {
            let json = match serde_json::to_string(entry) {
                Ok(json) => json,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        key = %canonical,
                        "SSR entry failed to serialize; not stored"
                    );
                    return;
                }
            };
            let mut conn = manager.clone();
            if let Err(err) = conn
                .set_ex::<_, _, ()>(&canonical, json, ttl.as_secs().max(1))
                .await
            {
                tracing::debug!(
                    error = %err,
                    key = %canonical,
                    "SSR cache write failed; next read renders"
                );
            }
            return;
        }
        let mut inner = self.local.write().expect("ssr local cache lock poisoned");
        if inner.entries.len() >= self.max_local_entries {
            inner
                .entries
                .retain(|_, local| local.stored_at.elapsed() < local.ttl);
            while inner.entries.len() >= self.max_local_entries {
                // Still full: drop the arbitrary quarter the map iterates
                // first. The cap protects memory; hit ratio is not a
                // contract here.
                let victims: Vec<String> = inner
                    .entries
                    .keys()
                    .take(inner.entries.len() - self.max_local_entries * 3 / 4)
                    .cloned()
                    .collect();
                for victim in victims {
                    inner.entries.remove(&victim);
                }
            }
        }
        inner.entries.insert(
            canonical,
            LocalEntry {
                entry: entry.clone(),
                stored_at: Instant::now(),
                ttl,
            },
        );
    }

    /// Platform-wide invalidation (a platform-scope content change — e.g. a
    /// platform translation — touches every tenant's fallback chain). Returns
    /// the number of entries removed.
    pub async fn invalidate_all(&self) -> anyhow::Result<u64> {
        let mut removed = 0u64;
        if let Some(manager) = &self.redis {
            let mut scan_conn = manager.clone();
            let mut conn = manager.clone();
            let mut iter = scan_conn
                .scan_match::<_, String>("sanvi:*:ssr:*")
                .await
                .context("failed to scan SSR cache for platform-wide invalidation")?;
            while let Some(item) = iter.next_item().await {
                let key = item.with_context(|| "failed while scanning SSR cache keys")?;
                let _: Result<(), _> = conn.del(&key).await;
                removed += 1;
            }
        }
        let mut inner = self.local.write().expect("ssr local cache lock poisoned");
        removed += inner.entries.len() as u64;
        inner.entries.clear();
        Ok(removed)
    }

    /// Event-driven invalidation (TASK-022 11.3d): drop every SSR entry of
    /// one tenant. Returns the number of entries removed.
    pub async fn invalidate_tenant(&self, tenant_id: TenantId) -> anyhow::Result<u64> {
        let mut removed = 0u64;
        let prefix = format!("sanvi:{tenant_id}:ssr:");
        if let Some(manager) = &self.redis {
            let mut scan_conn = manager.clone();
            let mut conn = manager.clone();
            let mut iter = scan_conn
                .scan_match::<_, String>(&ssr_invalidation_pattern(tenant_id))
                .await
                .context("failed to scan SSR cache for invalidation")?;
            while let Some(item) = iter.next_item().await {
                let key = item.with_context(|| "failed while scanning SSR cache keys")?;
                let _: Result<(), _> = conn.del(&key).await;
                removed += 1;
            }
        }
        let mut inner = self.local.write().expect("ssr local cache lock poisoned");
        let before = inner.entries.len();
        inner.entries.retain(|key, _| !key.starts_with(&prefix));
        removed += (before - inner.entries.len()) as u64;
        Ok(removed)
    }
}

/// The invalidation scan pattern for one tenant's SSR entries (event-driven
/// invalidation on theme publish / content change).
pub fn ssr_invalidation_pattern(tenant_id: TenantId) -> String {
    format!("sanvi:{tenant_id}:ssr:*")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sanvi_kernel::SnowflakeId;

    fn key(tenant: TenantId, locale: &str, revision: i32, device: DeviceClass) -> SsrCacheKey {
        SsrCacheKey::new(tenant, locale, revision, device, "/api/v1/public/theme").unwrap()
    }

    fn make_tenant(raw: i64) -> TenantId {
        TenantId::from_snowflake(SnowflakeId::try_from(raw).unwrap())
    }

    #[test]
    fn canonical_key_sits_under_the_tenant_namespace() {
        let tenant = make_tenant(0x7f01);
        let rendered = key(tenant, "en", 3, DeviceClass::Desktop).to_cache_key();
        assert_eq!(
            rendered,
            format!("sanvi:{tenant}:ssr:desktop/3/en?/api/v1/public/theme"),
            "{rendered}"
        );
        assert!(rendered.starts_with(&format!("sanvi:{tenant}:ssr:")));
    }

    #[test]
    fn an_invalid_locale_or_route_is_rejected_at_construction() {
        let tenant = make_tenant(0x7f02);
        let colon = SsrCacheKey::new(tenant, "en:GB", 1, DeviceClass::Desktop, "/x");
        assert_eq!(
            colon.unwrap_err(),
            SsrKeyError {
                name: "locale",
                reason: "expected a lowercase BCP-47 tag of at most 35 characters (a-z, 0-9, '-')",
            }
        );
        let too_long = SsrCacheKey::new(
            tenant,
            "en",
            1,
            DeviceClass::Desktop,
            format!("/x?{}", "a".repeat(600)),
        );
        assert!(too_long.is_err());
        let not_a_path =
            SsrCacheKey::new(tenant, "en", 1, DeviceClass::Desktop, "api/v1/public/theme");
        assert!(not_a_path.is_err());
    }

    #[test]
    fn percent_signs_do_not_ambiguate_the_route() {
        let tenant = make_tenant(0x7f03);
        let a = SsrCacheKey::new(tenant, "en", 1, DeviceClass::Desktop, "/x%2Fy").unwrap();
        let b = SsrCacheKey::new(tenant, "en", 1, DeviceClass::Desktop, "/x%252Fy").unwrap();
        assert_ne!(a.to_cache_key(), b.to_cache_key());
    }

    #[test]
    fn invalidation_pattern_covers_only_the_tenant_ssr_namespace() {
        let tenant = make_tenant(0x7f04);
        assert_eq!(
            ssr_invalidation_pattern(tenant),
            format!("sanvi:{tenant}:ssr:*")
        );
        assert_ne!(
            ssr_invalidation_pattern(tenant),
            ssr_invalidation_pattern(make_tenant(0x7f05))
        );
    }
}
