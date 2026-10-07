//! Outbox-backed event publishing and system clock (mirrors the other
//! contexts' adapter pattern).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::{EventId, RawIdGenerator};
use sanvi_messaging::OutboxWriter;

use crate::domain::LocalizationError;
use crate::domain::events::LocalizationEvent;
use crate::domain::ports::{Clock, EventPublisher};

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
        event: &LocalizationEvent,
    ) -> Result<sanvi_kernel::EventEnvelope, LocalizationError> {
        let id = self
            .ids
            .next_id()
            .map(EventId::from_snowflake)
            .map_err(|err| LocalizationError::Outbox(err.to_string()))?;
        Ok(event.envelope(id))
    }
}

#[async_trait::async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        event: &LocalizationEvent,
    ) -> Result<(), LocalizationError> {
        let envelope = self.envelope(event)?;
        OutboxWriter
            .append(scope.tx_mut(), &envelope)
            .await
            .map_err(|err| LocalizationError::Outbox(err.to_string()))
    }

    async fn publish_scoped(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        event: &LocalizationEvent,
    ) -> Result<(), LocalizationError> {
        let envelope = self.envelope(event)?;
        OutboxWriter
            .append(scope.tx_mut(), &envelope)
            .await
            .map_err(|err| LocalizationError::Outbox(err.to_string()))
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

/// The worker's consumer handler: invalidates the merged-catalog and tenant
/// settings caches when another process changes copy or locale settings, so
/// worker-rendered output (notifications, exports) never serves stale text.
/// Handlers are idempotent by construction (dedup key in
/// `platform.processed_events`), so re-delivery after a crash is safe.
pub fn localization_event_handler(
    service: std::sync::Arc<crate::adapters::catalog::LocalizationService>,
    ssr_cache: Option<std::sync::Arc<sanvi_cache::SsrCache>>,
) -> sanvi_messaging::ConsumerHandler {
    std::sync::Arc::new(move |envelope: sanvi_kernel::events::EventEnvelope| {
        let service = service.clone();
        let ssr_cache = ssr_cache.clone();
        Box::pin(async move {
            // TASK-022 (FR-1111): copy changes are the "content change" half
            // of the SSR invalidation contract — a tenant-scoped change drops
            // that tenant's entries, a platform-scoped one drops everything
            // (platform copy rides every tenant's fallback chain).
            match envelope.event_type.as_str() {
                "localization.tenant_locales_changed" => {
                    let tenant_id = envelope
                        .metadata
                        .get("tenant_id")
                        .and_then(|value| value.as_str())
                        .and_then(|value| value.parse::<sanvi_kernel::TenantId>().ok());
                    if let Some(tenant_id) = tenant_id {
                        service.invalidate_tenant(tenant_id);
                        if let Some(ssr) = &ssr_cache {
                            ssr.invalidate_tenant(tenant_id).await?;
                        }
                    }
                    service.cache().clear();
                }
                "localization.translation_updated" | "localization.translation_deleted" => {
                    let scope = envelope
                        .metadata
                        .get("tenant_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("platform");
                    service.invalidate_scope(scope);
                    if let Some(ssr) = &ssr_cache {
                        match scope.parse::<sanvi_kernel::TenantId>() {
                            Ok(tenant_id) => {
                                ssr.invalidate_tenant(tenant_id).await?;
                            }
                            Err(_) => {
                                ssr.invalidate_all().await?;
                            }
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })
    })
}
