//! Outbox publishing, the system clock, and the worker's event handler.
//!
//! The handler's job is cache coherence: every event that changes what a
//! hostname resolves to (verified, live, removed) invalidates the shared
//! and local host→tenant caches, so the tenancy resolution hook never
//! serves a stale mapping.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use sanvi_kernel::{EventEnvelope, EventId, RawIdGenerator, TenantId};
use sanvi_messaging::{ConsumerHandler, OutboxWriter};
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::{Clock, DomainsError, DomainsEvent, EventPublisher};

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
        event: &DomainsEvent,
        tenant_id: TenantId,
    ) -> Result<EventEnvelope, DomainsError> {
        let id = self
            .ids
            .next_id()
            .map(EventId::from_snowflake)
            .map_err(|err| DomainsError::Outbox(err.to_string()))?;
        Ok(event.envelope(id, tenant_id))
    }
}

#[async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        events: &[DomainsEvent],
    ) -> Result<(), DomainsError> {
        let writer = OutboxWriter;
        for event in events {
            let envelope = self.envelope(event, tenant_id)?;
            writer
                .append(scope.tx_mut(), &envelope)
                .await
                .map_err(|err| DomainsError::Outbox(err.to_string()))?;
        }
        Ok(())
    }

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        events: &[DomainsEvent],
    ) -> Result<(), DomainsError> {
        let writer = OutboxWriter;
        for event in events {
            let envelope = self.envelope(event, tenant_id)?;
            writer
                .append(scope.tx_mut(), &envelope)
                .await
                .map_err(|err| DomainsError::Outbox(err.to_string()))?;
        }
        Ok(())
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

/// The worker's consumer handler: events that change resolution invalidate
/// the host→tenant cache in this instance's Redis/local cache. Idempotent
/// by construction (`platform.processed_events` dedup).
pub fn domains_event_handler(
    cache: Arc<crate::adapters::cache::LiveDomainCache>,
) -> ConsumerHandler {
    Arc::new(move |envelope: sanvi_kernel::events::EventEnvelope| {
        let cache = cache.clone();
        Box::pin(async move {
            if !envelope.event_type.starts_with("domains.") {
                return Ok(());
            }
            let invalidates = matches!(
                envelope.event_type.as_str(),
                "domains.domain_verified"
                    | "domains.domain_went_live"
                    | "domains.domain_removed"
                    | "domains.domain_degraded"
            );
            if !invalidates {
                return Ok(());
            }
            let Some(hostname) = envelope
                .payload
                .get("hostname")
                .and_then(|value| value.as_str())
            else {
                return Ok(());
            };
            cache
                .invalidate(hostname)
                .await
                .map_err(|err| anyhow::anyhow!(err))
        })
    })
}
