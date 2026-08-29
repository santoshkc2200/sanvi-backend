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
) -> sanvi_messaging::ConsumerHandler {
    std::sync::Arc::new(move |envelope: sanvi_kernel::events::EventEnvelope| {
        let cache = cache.clone();
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
