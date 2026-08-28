//! Driven adapters for the event, id and clock ports.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::Arc;

use sanvi_kernel::{EventEnvelope, EventId, RawIdGenerator};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::events::IdentityEvent;
use crate::domain::invitation::InvitationId;
use crate::domain::user::UserId;
use crate::domain::{Clock, EventPublisher, IdGenerator, IdentityError};

/// Publishes identity events into the transactional outbox (same transaction
/// as the state change they describe).
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
        events: &[IdentityEvent],
    ) -> Result<(), IdentityError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &self.envelope(event)?)
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
                .append(scope.tx_mut(), &self.envelope(event)?)
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

impl OutboxEventPublisher {
    fn envelope(&self, event: &IdentityEvent) -> Result<EventEnvelope, IdentityError> {
        self.ids.next_id().map(EventId::from_snowflake).map(|id| event.envelope(id)).map_err(Into::into)
    }
}

/// First-party IDs delegated to the process-wide raw generator.
#[derive(Clone)]
pub struct SystemIdGenerator {
    ids: Arc<dyn RawIdGenerator>,
}

impl SystemIdGenerator {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self { Self { ids } }
}

impl IdGenerator for SystemIdGenerator {
    fn next_user_id(&self) -> Result<UserId, IdentityError> {
        self.ids.next_id().map(UserId::from_snowflake).map_err(IdentityError::from)
    }

    fn next_invitation_id(&self) -> Result<InvitationId, IdentityError> {
        self.ids.next_id().map(InvitationId::from_snowflake).map_err(IdentityError::from)
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
