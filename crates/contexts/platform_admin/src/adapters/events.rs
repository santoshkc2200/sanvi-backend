//! The platform-admin event publisher: appends events to the transactional
//! outbox in the caller's transaction.

use async_trait::async_trait;
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::PlatformScope;

use crate::domain::PlatformAdminError;
use crate::domain::events::PlatformAdminEvent;
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
        events: &[PlatformAdminEvent],
    ) -> Result<(), PlatformAdminError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope())
                .await
                .map_err(|err| PlatformAdminError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}
