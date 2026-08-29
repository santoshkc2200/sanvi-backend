use std::sync::Arc;

use async_trait::async_trait;
use sanvi_kernel::{EventEnvelope, EventId, RawIdGenerator};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::AccessError;
use crate::domain::events::AccessEvent;
use crate::domain::ports::EventPublisher;

#[derive(Clone)]
pub struct OutboxEventPublisher {
    ids: Arc<dyn RawIdGenerator>,
}

impl OutboxEventPublisher {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }

    fn envelope(&self, event: &AccessEvent) -> Result<EventEnvelope, AccessError> {
        let id = self
            .ids
            .next_id()
            .map(EventId::from_snowflake)
            .map_err(|err| AccessError::IdGeneration(err.to_string()))?;
        Ok(event.envelope(id))
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
                .append(scope.tx_mut(), &self.envelope(event)?)
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
                .append(scope.tx_mut(), &self.envelope(event)?)
                .await
                .map_err(|err| AccessError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}
