//! Transactional outbox registration for advertising events.

use async_trait::async_trait;
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::ScopedTx;
use uuid::Uuid;

use crate::domain::{AdEvent, AdvertisingError, EventPublisher};

#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxEventPublisher;

impl OutboxEventPublisher {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        events: &[AdEvent],
    ) -> Result<(), AdvertisingError> {
        let writer = OutboxWriter;
        for event in events {
            writer
                .append(scope.tx_mut(), &event.envelope(tenant_id))
                .await
                .map_err(|error| AdvertisingError::PlatformUnavailable(error.to_string()))?;
        }
        Ok(())
    }
}
