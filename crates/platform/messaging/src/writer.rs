//! Transactional outbox writer: the application-side half of the outbox.
//!
//! Use cases call [`OutboxWriter::append`] inside the same transaction as the
//! state change that produced the event, so the event and its source state
//! commit or roll back together. The dispatcher (in `apps/worker`) relays
//! unpublished rows to Redis Streams.

use anyhow::{Context, Result};
use sanvi_kernel::EventEnvelope;
use sanvi_persistence::Tx;

/// Writes domain events to `platform.outbox`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxWriter;

impl OutboxWriter {
    /// Append one event to the outbox within the given transaction.
    pub async fn append(&self, tx: &mut Tx<'_>, envelope: &EventEnvelope) -> Result<()> {
        sqlx::query!(
            "INSERT INTO platform.outbox
               (id, aggregate, aggregate_id, event_type, payload, metadata, occurred_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            envelope.id,
            envelope.aggregate,
            envelope.aggregate_id,
            envelope.event_type,
            envelope.payload,
            envelope.metadata,
            envelope.occurred_at
        )
        .execute(&mut **tx.as_inner())
        .await
        .context("failed to append event to outbox")?;
        Ok(())
    }
}
