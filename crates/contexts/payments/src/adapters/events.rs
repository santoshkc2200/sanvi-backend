//! Outbox publishing for the payments context.

use async_trait::async_trait;

use sanvi_messaging::OutboxWriter;
use sanvi_persistence::{PlatformScope, ScopedTx};
use uuid::Uuid;

use crate::domain::{EventPublisher, PaymentsError, PaymentsEvent};

#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxEventPublisher;

impl OutboxEventPublisher {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        events: &[PaymentsEvent],
    ) -> Result<(), PaymentsError> {
        let writer = OutboxWriter;
        for event in events {
            let envelope = event.envelope(tenant_id);
            writer
                .append(scope.tx_mut(), &envelope)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        events: &[PaymentsEvent],
    ) -> Result<(), PaymentsError> {
        let writer = OutboxWriter;
        for event in events {
            let envelope = event.envelope(tenant_id);
            writer
                .append(scope.tx_mut(), &envelope)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}
