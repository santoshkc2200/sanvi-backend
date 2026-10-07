//!
//! External-effect consumer handler: cache invalidation only, no database
//! transaction, so it runs on `run_until_shutdown` rather than the
//! transactional path. Redelivery is safe — invalidating an already-invalid
//! entry is a no-op.
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::{EventId, RawIdGenerator, TenantId};
use sanvi_messaging::OutboxWriter;

use crate::adapters::cache::ResolvedThemeCache;
use crate::domain::ThemingError;
use crate::domain::events::ThemingEvent;
use crate::domain::ports::{Clock, EventPublisher, IdGenerator};
use crate::domain::theme::{ThemeId, ThemeVersionId};

#[derive(Clone)]
pub struct OutboxEventPublisher {
    ids: Arc<dyn RawIdGenerator>,
}

impl OutboxEventPublisher {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }

    fn envelope(
        &self,
        event: &ThemingEvent,
    ) -> Result<sanvi_kernel::events::EventEnvelope, ThemingError> {
        let id = self.ids.next_id().map(EventId::from_snowflake)?;
        Ok(event.envelope(id))
    }
}

#[async_trait::async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        event: &ThemingEvent,
    ) -> Result<(), ThemingError> {
        let envelope = self.envelope(event)?;
        OutboxWriter
            .append(scope.tx_mut(), &envelope)
            .await
            .map_err(|err| ThemingError::Outbox(err.to_string()))
    }

    async fn publish_scoped(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        event: &ThemingEvent,
    ) -> Result<(), ThemingError> {
        let envelope = self.envelope(event)?;
        OutboxWriter
            .append(scope.tx_mut(), &envelope)
            .await
            .map_err(|err| ThemingError::Outbox(err.to_string()))
    }
}

#[derive(Clone)]
pub struct SystemIdGenerator {
    ids: Arc<dyn RawIdGenerator>,
}

impl SystemIdGenerator {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }
}

impl IdGenerator for SystemIdGenerator {
    fn next_theme_id(&self) -> Result<ThemeId, ThemingError> {
        self.ids
            .next_id()
            .map(ThemeId::from_snowflake)
            .map_err(|err| ThemingError::IdGeneration(err.to_string()))
    }

    fn next_theme_version_id(&self) -> Result<ThemeVersionId, ThemingError> {
        self.ids
            .next_id()
            .map(ThemeVersionId::from_snowflake)
            .map_err(|err| ThemingError::IdGeneration(err.to_string()))
    }
}

/// Real wall clock (UTC).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// The worker's consumer handler: any event that changes what a tenant's
/// resolved theme looks like invalidates the tenant's cache entries in this
/// instance's Redis/local cache. Idempotent by construction (the
/// `platform.processed_events` dedup), so re-delivery is safe.
pub fn theming_event_handler(
    cache: std::sync::Arc<ResolvedThemeCache>,
    ssr_cache: Option<std::sync::Arc<sanvi_cache::SsrCache>>,
) -> sanvi_messaging::ConsumerHandler {
    std::sync::Arc::new(move |envelope: sanvi_kernel::events::EventEnvelope| {
        let cache = cache.clone();
        let ssr_cache = ssr_cache.clone();
        Box::pin(async move {
            match envelope.event_type.as_str() {
                "theming.tenant_theme_published"
                | "theming.tenant_theme_rolled_back"
                | "theming.tenant_theme_changed"
                | "theming.brand_asset_updated" => {
                    let tenant_id = envelope
                        .metadata
                        .get("tenant_id")
                        .and_then(|value| value.as_str())
                        .and_then(|value| value.parse::<TenantId>().ok());
                    if let Some(tenant_id) = tenant_id {
                        cache.invalidate_tenant(tenant_id).await;
                        // TASK-022 (FR-1111): the storefront SSR cache
                        // partitions by theme revision, so a publish makes
                        // the old entries unreachable; dropping them keeps
                        // the documented ~2 s window honest and frees the
                        // memory the backstop TTL would have held.
                        if let Some(ssr) = &ssr_cache {
                            ssr.invalidate_tenant(tenant_id).await?;
                        }
                    }
                }
                // Registry events don't change any tenant's rendered theme:
                // pinned tenants keep rendering a yanked version, and a new
                // version only applies when a tenant selects it.
                "theming.theme_published" | "theming.theme_yanked" => {}
                _ => {}
            }
            Ok(())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sanvi_cache::{DeviceClass, SsrCache, SsrCacheKey};
    use sanvi_kernel::SnowflakeId;

    fn envelope(event_type: &str, tenant_id: TenantId) -> sanvi_kernel::events::EventEnvelope {
        sanvi_kernel::events::EventEnvelope::new(
            EventId::from_snowflake(SnowflakeId::try_from(42).unwrap()),
            "theming.tenant_theme",
            tenant_id.as_snowflake(),
            event_type,
            serde_json::json!({}),
        )
        .with_metadata("tenant_id", serde_json::json!(tenant_id.to_string()))
    }

    async fn seed_entry(ssr: &SsrCache, tenant_id: TenantId, route: &str) {
        let key =
            SsrCacheKey::new(tenant_id, "en", 3, DeviceClass::Desktop, route).expect("seed key");
        let entry = sanvi_cache::SsrEntry {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        };
        ssr.put(&key, &entry, std::time::Duration::from_secs(300))
            .await;
    }

    /// A theme publish must drop the tenant's SSR entries inside the
    /// documented ~2 s window (TASK-022 11.3d) — here: immediately, the
    /// handler running synchronously — while another tenant's entries
    /// survive.
    #[tokio::test]
    async fn theme_publish_invalidates_the_tenant_ssr_entries_within_the_window() {
        let published = TenantId::from_snowflake(SnowflakeId::try_from(300).unwrap());
        let untouched = TenantId::from_snowflake(SnowflakeId::try_from(301).unwrap());
        let ssr = Arc::new(SsrCache::new(None, None));
        seed_entry(&ssr, published, "/api/v1/public/theme").await;
        seed_entry(&ssr, published, "/api/v1/public/plans").await;
        seed_entry(&ssr, untouched, "/api/v1/public/theme").await;

        let handler = theming_event_handler(
            Arc::new(ResolvedThemeCache::new(None)),
            Some(Arc::clone(&ssr)),
        );
        handler(envelope("theming.tenant_theme_published", published))
            .await
            .expect("handler succeeds");

        let removed = ssr.invalidate_tenant(published).await.unwrap();
        assert_eq!(
            removed, 0,
            "the published tenant's entries are already gone"
        );
        let remaining = ssr.invalidate_tenant(untouched).await.unwrap();
        assert_eq!(
            remaining, 1,
            "another tenant's entries survive the invalidation"
        );
    }

    /// A tenant key the handler cannot parse must not nuke the world, and
    /// unrelated events are inert.
    #[tokio::test]
    async fn unrelated_events_leave_the_ssr_cache_alone() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(302).unwrap());
        let ssr = Arc::new(SsrCache::new(None, None));
        seed_entry(&ssr, tenant_id, "/api/v1/public/theme").await;

        let handler = theming_event_handler(
            Arc::new(ResolvedThemeCache::new(None)),
            Some(Arc::clone(&ssr)),
        );
        // A registry event: no tenant's rendering changes, nothing dropped.
        handler(envelope("theming.theme_published", tenant_id))
            .await
            .expect("handler succeeds");
        let removed = ssr.invalidate_tenant(tenant_id).await.unwrap();
        assert_eq!(
            removed, 1,
            "registry events do not invalidate the SSR cache"
        );
    }
}
