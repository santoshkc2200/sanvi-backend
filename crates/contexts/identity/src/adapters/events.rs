//! Driven adapters for the event, id and clock ports.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::events::IdentityEvent;
use crate::domain::invitation::InvitationId;
use crate::domain::user::UserId;
use crate::domain::{Clock, EventPublisher, IdGenerator, IdentityError};

/// Publishes identity events into the transactional outbox (same transaction
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
        events: &[IdentityEvent],
    ) -> Result<(), IdentityError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope())
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        events: &[IdentityEvent],
    ) -> Result<(), IdentityError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope())
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn publish_envelopes_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        envelopes: &[EventEnvelope],
    ) -> Result<(), IdentityError> {
        for envelope in envelopes {
            OutboxWriter
                .append(scope.tx_mut(), envelope)
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}

/// Real id generation (UUIDv7 via the shared kernel).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemIdGenerator;

impl IdGenerator for SystemIdGenerator {
    fn next_user_id(&self) -> UserId {
        UserId::new()
    }

    fn next_invitation_id(&self) -> InvitationId {
        InvitationId::new()
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
