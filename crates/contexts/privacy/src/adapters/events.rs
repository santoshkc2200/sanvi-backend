//! The outbox event publisher (same transaction as the state change).

use async_trait::async_trait;
use sanvi_kernel::{EventId, RawIdGenerator};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::PlatformScope;
use std::sync::Arc;

use crate::domain::{EventPublisher, PrivacyError, PrivacyEvent};

#[derive(Clone)]
pub struct OutboxEventPublisher {
    ids: Arc<dyn RawIdGenerator>,
}

impl OutboxEventPublisher {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }

    fn envelope(&self, event: &PrivacyEvent) -> Result<sanvi_kernel::EventEnvelope, PrivacyError> {
        let id = self.ids.next_id().map(EventId::from_snowflake)?;
        Ok(event.envelope(id, event.tenant_id()))
    }
}

#[async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        events: &[PrivacyEvent],
    ) -> Result<(), PrivacyError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &self.envelope(event)?)
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}
