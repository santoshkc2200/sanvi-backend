//! The billing event publisher (outbox) and the system clock/id generator.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::PlatformScope;

use crate::domain::events::BillingEvent;
use crate::domain::{
    BillingError, Clock, EventPublisher, IdGenerator, InvoiceId, PlanId, PriceId, SubscriptionId,
};

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
        events: &[BillingEvent],
    ) -> Result<(), BillingError> {
        for event in events {
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope())
                .await
                .map_err(|err| BillingError::Storage(err.to_string()))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl SystemClock {
    pub fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemIdGenerator;

impl SystemIdGenerator {
    pub fn new() -> Self {
        Self
    }
}

impl IdGenerator for SystemIdGenerator {
    fn next_plan_id(&self) -> PlanId {
        PlanId::new()
    }

    fn next_price_id(&self) -> PriceId {
        PriceId::new()
    }

    fn next_subscription_id(&self) -> SubscriptionId {
        SubscriptionId::new()
    }

    fn next_invoice_id(&self) -> InvoiceId {
        InvoiceId::new()
    }
}
