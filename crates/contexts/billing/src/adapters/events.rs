//! The billing event publisher (outbox) and the system clock/id generator.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::{EventId, RawIdGenerator};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::PlatformScope;

use crate::domain::events::BillingEvent;
use crate::domain::{
    BillingError, Clock, EventPublisher, IdGenerator, InvoiceId, PlanId, PriceId, SubscriptionId,
};

#[derive(Clone)]
pub struct OutboxEventPublisher {
    ids: Arc<dyn RawIdGenerator>,
}

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
        events: &[BillingEvent],
    ) -> Result<(), BillingError> {
        for event in events {
            let id = self
                .ids
                .next_id()
                .map(EventId::from_snowflake)
                .map_err(|err| BillingError::Storage(err.to_string()))?;
            OutboxWriter
                .append(scope.tx_mut(), &event.envelope(id))
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

#[derive(Clone)]
pub struct SystemIdGenerator {
    ids: Arc<dyn RawIdGenerator>,
}

impl SystemIdGenerator {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }

    fn next(&self) -> Result<sanvi_kernel::SnowflakeId, BillingError> {
        self.ids
            .next_id()
            .map_err(|err| BillingError::Storage(err.to_string()))
    }
}

impl IdGenerator for SystemIdGenerator {
    fn next_plan_id(&self) -> Result<PlanId, BillingError> {
        self.next().map(PlanId::from_snowflake)
    }

    fn next_price_id(&self) -> Result<PriceId, BillingError> {
        self.next().map(PriceId::from_snowflake)
    }

    fn next_subscription_id(&self) -> Result<SubscriptionId, BillingError> {
        self.next().map(SubscriptionId::from_snowflake)
    }

    fn next_invoice_id(&self) -> Result<InvoiceId, BillingError> {
        self.next().map(InvoiceId::from_snowflake)
    }
}
