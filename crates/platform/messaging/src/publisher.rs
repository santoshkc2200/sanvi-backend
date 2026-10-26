//! Delivery adapters keep context event writers independent of brokers.

use std::{future::Future, pin::Pin};

use anyhow::Result;
use sanvi_kernel::EventEnvelope;

use crate::StreamsClient;

pub type PublishFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// A successful publish makes an event available for at-least-once consumption.
/// NATS/Kafka adapters must wait for the broker's durable acknowledgement here.
/// Consumers must deduplicate by `EventEnvelope::id`.
pub trait EventPublisher: Send + Sync {
    fn publish<'a>(&'a self, destination: &'a str, event: &'a EventEnvelope) -> PublishFuture<'a>;
}

/// Publication becomes visible to PostgreSQL consumers when the dispatcher
/// commits `published_at` on the retained outbox row. No second write is needed.
pub struct PostgresPublisher;

impl EventPublisher for PostgresPublisher {
    fn publish<'a>(&'a self, _: &'a str, _: &'a EventEnvelope) -> PublishFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

impl EventPublisher for StreamsClient {
    fn publish<'a>(&'a self, destination: &'a str, event: &'a EventEnvelope) -> PublishFuture<'a> {
        Box::pin(async move {
            let payload = serde_json::to_string(event)?;
            self.append_durable(destination, &[("event", payload.as_str())])
                .await?;
            Ok(())
        })
    }
}
