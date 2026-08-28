//! The platform-admin event publisher: appends events to the transactional
//! outbox in the caller's transaction.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_kernel::{EventId, RawIdGenerator};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::PlatformScope;

use crate::domain::PlatformAdminError;
use crate::domain::events::PlatformAdminEvent;
use crate::domain::ports::EventPublisher;

#[derive(Clone)]
pub struct OutboxEventPublisher { ids: Arc<dyn RawIdGenerator> }

impl OutboxEventPublisher {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
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
                .append(scope.tx_mut(), &self.envelope(event)?)
                .await
                .map_err(|err| PlatformAdminError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}

impl OutboxEventPublisher {
    fn envelope(&self, event: &PlatformAdminEvent) -> Result<sanvi_kernel::EventEnvelope, PlatformAdminError> {
        self.ids.next_id().map(EventId::from_snowflake).map(|id| event.envelope(id)).map_err(Into::into)
    }
}
