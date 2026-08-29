//! Outbox publishing for the payments context.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_kernel::events::EventEnvelope;
use sanvi_kernel::{EventId, RawIdGenerator, TenantId};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::{
    CheckoutId, ConnectionId, DisputeId, EventPublisher, IdGenerator, PaymentId, PaymentsError,
    PaymentsEvent, PayoutId, RefundId,
};

#[derive(Clone)]
pub struct OutboxEventPublisher {
    ids: Arc<dyn RawIdGenerator>,
}

impl OutboxEventPublisher {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }

    fn envelope(
        &self,
        event: &PaymentsEvent,
        tenant_id: TenantId,
    ) -> Result<EventEnvelope, PaymentsError> {
        let event_id = self
            .ids
            .next_id()
            .map(EventId::from_snowflake)
            .map_err(|err| PaymentsError::IdGeneration(err.to_string()))?;
        Ok(event.envelope(event_id, tenant_id))
    }
}

#[async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        events: &[PaymentsEvent],
    ) -> Result<(), PaymentsError> {
        let writer = OutboxWriter;
        for event in events {
            let envelope = self.envelope(event, tenant_id)?;
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
        tenant_id: TenantId,
        events: &[PaymentsEvent],
    ) -> Result<(), PaymentsError> {
        let writer = OutboxWriter;
        for event in events {
            let envelope = self.envelope(event, tenant_id)?;
            writer
                .append(scope.tx_mut(), &envelope)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SystemIdGenerator {
    ids: Arc<dyn RawIdGenerator>,
}

impl SystemIdGenerator {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }
}

impl IdGenerator for SystemIdGenerator {
    fn next_connection_id(&self) -> Result<ConnectionId, PaymentsError> {
        self.ids
            .next_id()
            .map(ConnectionId::from_snowflake)
            .map_err(|err| PaymentsError::IdGeneration(err.to_string()))
    }

    fn next_checkout_id(&self) -> Result<CheckoutId, PaymentsError> {
        self.ids
            .next_id()
            .map(CheckoutId::from_snowflake)
            .map_err(|err| PaymentsError::IdGeneration(err.to_string()))
    }

    fn next_refund_id(&self) -> Result<RefundId, PaymentsError> {
        self.ids
            .next_id()
            .map(RefundId::from_snowflake)
            .map_err(|err| PaymentsError::IdGeneration(err.to_string()))
    }

    fn next_payment_id(&self) -> Result<PaymentId, PaymentsError> {
        self.ids
            .next_id()
            .map(PaymentId::from_snowflake)
            .map_err(|err| PaymentsError::IdGeneration(err.to_string()))
    }

    fn next_dispute_id(&self) -> Result<DisputeId, PaymentsError> {
        self.ids
            .next_id()
            .map(DisputeId::from_snowflake)
            .map_err(|err| PaymentsError::IdGeneration(err.to_string()))
    }

    fn next_payout_id(&self) -> Result<PayoutId, PaymentsError> {
        self.ids
            .next_id()
            .map(PayoutId::from_snowflake)
            .map_err(|err| PaymentsError::IdGeneration(err.to_string()))
    }
}
