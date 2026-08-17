//! Outbox-backed event publishing and system clock (mirrors the other
//! contexts' adapter pattern).

use chrono::{DateTime, Utc};
use sanvi_messaging::OutboxWriter;

use crate::domain::LocalizationError;
use crate::domain::events::LocalizationEvent;
use crate::domain::ports::{Clock, EventPublisher};

#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxEventPublisher;

impl OutboxEventPublisher {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        event: &LocalizationEvent,
    ) -> Result<(), LocalizationError> {
        let envelope = event.envelope();
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
        let envelope = event.envelope();
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
) -> sanvi_messaging::ConsumerHandler {
    std::sync::Arc::new(move |envelope: sanvi_kernel::events::EventEnvelope| {
        let service = service.clone();
        Box::pin(async move {
            match envelope.event_type.as_str() {
                "localization.tenant_locales_changed" => {
                    let tenant_id = envelope
                        .metadata
                        .get("tenant_id")
                        .and_then(|value| value.as_str())
                        .and_then(|value| uuid::Uuid::parse_str(value).ok());
                    if let Some(tenant_id) = tenant_id {
                        service.invalidate_tenant(tenant_id);
                    }
                    service.cache().clear();
                }
                "localization.translation_updated" => {
                    let scope = envelope
                        .metadata
                        .get("tenant_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("platform");
                    service.invalidate_scope(scope);
                }
                _ => {}
            }
            Ok(())
        })
    })
}
