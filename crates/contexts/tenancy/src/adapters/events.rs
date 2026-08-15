//! Driven adapters for the event, id and clock ports — plus the consumer
//! that keeps caches coherent when tenant events arrive over the stream.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use sanvi_messaging::{ConsumerHandler, OutboxWriter};
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::events::TenantEvent;
use crate::domain::tenant::TenantId;
use crate::domain::{Clock, EventPublisher, IdGenerator, TenancyError};

/// Publishes tenant events into the transactional outbox (same transaction
/// as the state change they describe).
#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxEventPublisher;

impl OutboxEventPublisher {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        events: &[TenantEvent],
    ) -> Result<(), TenancyError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope())
                .await
                .map_err(|err| TenancyError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        events: &[TenantEvent],
    ) -> Result<(), TenancyError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope())
                .await
                .map_err(|err| TenancyError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn publish_envelopes_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        envelopes: &[EventEnvelope],
    ) -> Result<(), TenancyError> {
        for envelope in envelopes {
            OutboxWriter
                .append(scope.tx_mut(), envelope)
                .await
                .map_err(|err| TenancyError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}

/// Real id generation (UUIDv7 via the shared kernel).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemIdGenerator;

impl IdGenerator for SystemIdGenerator {
    fn next_tenant_id(&self) -> TenantId {
        TenantId::new()
    }
}

/// Real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Build the worker's tenancy consumer: on lifecycle events (anything that
/// changes the cached resolution context — status, slug mapping, region) it
/// invalidates the given resolution cache so the middleware never serves a
/// stale tenant. Events that do not affect the resolution context (settings
/// changes) are no-ops.
///
/// Handlers are idempotent by construction (dedup key in
/// `platform.processed_events`), so re-delivery after a crash is safe.
pub fn tenant_event_handler(
    invalidate: impl Fn(String, uuid::Uuid) + Send + Sync + 'static,
) -> ConsumerHandler {
    let invalidate = std::sync::Arc::new(invalidate);
    std::sync::Arc::new(move |envelope: EventEnvelope| {
        let invalidate = invalidate.clone();
        let invalidate_slug = envelope
            .payload
            .get("slug")
            .and_then(|slug| slug.as_str())
            .map(str::to_string);
        let tenant_id = envelope
            .metadata
            .get("tenant_id")
            .and_then(|id| id.as_str())
            .and_then(|id| id.parse::<uuid::Uuid>().ok())
            .unwrap_or(envelope.aggregate_id);

        Box::pin(async move {
            match invalidate_slug {
                Some(slug) => {
                    invalidate(slug, tenant_id);
                    Ok(())
                }
                // No slug in the payload (e.g. settings changes): nothing in
                // the resolution context changed — nothing to invalidate.
                None => Ok(()),
            }
        })
    })
}
