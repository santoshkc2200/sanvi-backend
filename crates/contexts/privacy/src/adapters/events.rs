//! The outbox event publisher (same transaction as the state change).

use async_trait::async_trait;
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::PlatformScope;

use crate::domain::{EventPublisher, PrivacyError, PrivacyEvent};

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
        events: &[PrivacyEvent],
    ) -> Result<(), PrivacyError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope(None))
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}
