//! The access context's event publisher: appends events to the
//! transactional outbox in the caller's transaction (PlatformScope or
//! tenant-scoped).

use async_trait::async_trait;
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::AccessError;
use crate::domain::events::AccessEvent;
use crate::domain::ports::EventPublisher;

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
        events: &[AccessEvent],
    ) -> Result<(), AccessError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope())
                .await
                .map_err(|err| AccessError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        events: &[AccessEvent],
    ) -> Result<(), AccessError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope())
                .await
                .map_err(|err| AccessError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}
