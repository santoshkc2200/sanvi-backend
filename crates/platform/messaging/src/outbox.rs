use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::{error, info};
use uuid::Uuid;

use crate::redis::StreamsClient;

/// Name of the stream domain events are published to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamName(pub String);

impl Default for StreamName {
    fn default() -> Self {
        Self("sanvi.events".to_string())
    }
}

impl StreamName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Row shape of `platform.outbox`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub id: Uuid,
    pub aggregate: String,
    pub aggregate_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
    pub attempts: i32,
}

impl OutboxRecord {
    pub fn envelope(&self) -> EventEnvelope {
        EventEnvelope {
            id: self.id,
            aggregate: self.aggregate.clone(),
            aggregate_id: self.aggregate_id,
            event_type: self.event_type.clone(),
            payload: self.payload.clone(),
            metadata: self.metadata.clone(),
            occurred_at: self.occurred_at,
        }
    }
}

/// Relays unpublished outbox rows to Redis Streams.
///
/// - Polls with `FOR UPDATE SKIP LOCKED` so multiple workers can share the
///   queue without double-delivery.
/// - Publishes then marks published; a crash between the two is safe because
///   consumers deduplicate on the event id.
/// - Failed publishes increment `attempts`; rows are retried on the next
///   poll. A row is abandoned (logged) after `max_attempts` so a poison
///   message cannot block the queue forever.
#[derive(Clone)]
pub struct OutboxDispatcher {
    pool: PgPool,
    streams: StreamsClient,
    stream: StreamName,
    batch_size: usize,
    max_attempts: i32,
}

impl OutboxDispatcher {
    pub fn new(
        pool: PgPool,
        streams: StreamsClient,
        stream: StreamName,
        batch_size: usize,
        max_attempts: i32,
    ) -> Self {
        Self {
            pool,
            streams,
            stream,
            batch_size,
            max_attempts,
        }
    }

    /// Claim one batch of unpublished rows, holding their locks for the life
    /// of the returned transaction.
    ///
    /// `SELECT ... FOR UPDATE SKIP LOCKED` only protects against concurrent
    /// claims (by another worker replica or another poll cycle) for as long
    /// as the row locks are held — that requires running it, and every
    /// following `mark_published`/`record_failure`, inside the *same*
    /// transaction. A bare `.fetch_all(&pool)` auto-commits the SELECT the
    /// instant it returns, releasing the locks before the batch is
    /// processed, which lets two dispatchers claim and publish the same
    /// rows.
    async fn claim(&self) -> Result<(sqlx::Transaction<'_, sqlx::Postgres>, Vec<OutboxRecord>)> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("failed to start outbox claim transaction")?;
        let rows = sqlx::query_as!(
            OutboxRecord,
            r#"SELECT id, aggregate, aggregate_id, event_type, payload, metadata,
                      occurred_at, published_at, attempts
               FROM platform.outbox
               WHERE published_at IS NULL
                 AND attempts < $1
               ORDER BY occurred_at
               LIMIT $2
               FOR UPDATE SKIP LOCKED"#,
            self.max_attempts,
            self.batch_size as i64
        )
        .fetch_all(&mut *tx)
        .await
        .context("failed to claim outbox batch")?;
        Ok((tx, rows))
    }

    async fn mark_published(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE platform.outbox SET published_at = now() WHERE id = $1",
            id
        )
        .execute(&mut **tx)
        .await
        .context("failed to mark outbox row published")?;
        Ok(())
    }

    async fn record_failure(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE platform.outbox SET attempts = attempts + 1 WHERE id = $1",
            id
        )
        .execute(&mut **tx)
        .await
        .context("failed to record outbox failure")?;
        Ok(())
    }

    /// Publish one batch. Returns the number of events published.
    ///
    /// The whole batch — claim, every publish attempt, every mark — runs
    /// inside one transaction, so the `FOR UPDATE SKIP LOCKED` claim stays
    /// in effect (and the rows genuinely unavailable to a concurrent
    /// dispatcher) until the batch is fully processed and committed.
    pub async fn run_once(&self) -> Result<usize> {
        let (mut tx, batch) = self.claim().await?;
        if batch.is_empty() {
            tx.commit().await.context("failed to close empty claim")?;
            return Ok(0);
        }

        let mut published = 0;
        for record in batch {
            let envelope = record.envelope();
            let payload =
                serde_json::to_string(&envelope).context("failed to serialize event envelope")?;

            match self
                .streams
                .append(self.stream.as_str(), &[("event", payload.as_str())], 10_000)
                .await
            {
                Ok(_) => {
                    if let Err(err) = Self::mark_published(&mut tx, record.id).await {
                        // The event reached the stream; a failed mark means it
                        // will be delivered again — consumers dedupe on id.
                        error!(error = %err, event_id = %record.id, "published but failed to mark");
                        continue;
                    }
                    published += 1;
                }
                Err(err) => {
                    error!(error = %err, event_id = %record.id, attempts = record.attempts + 1,
                        "failed to publish event to stream");
                    if let Err(err) = Self::record_failure(&mut tx, record.id).await {
                        error!(error = %err, event_id = %record.id, "failed to record outbox failure");
                    }
                }
            }
        }
        tx.commit().await.context("failed to commit outbox batch")?;
        Ok(published)
    }

    /// Poll in a loop until `shutdown` fires; exits after the current batch.
    pub async fn run_until_shutdown(
        &self,
        poll_interval: Duration,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        info!(stream = self.stream.as_str(), interval = ?poll_interval, "outbox dispatcher started");
        loop {
            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
            }
            match self.run_once().await {
                Ok(0) => {}
                Ok(n) => info!(count = n, "published outbox batch"),
                Err(err) => error!(error = %err, "outbox dispatch failed"),
            }
        }
        info!("outbox dispatcher stopped");
    }
}
