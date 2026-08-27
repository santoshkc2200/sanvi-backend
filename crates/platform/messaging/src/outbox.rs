use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sanvi_kernel::{EventEnvelope, EventId, SnowflakeId};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use tracing::{error, info};

use crate::{OutboxMetrics, redis::StreamsClient};

/// Base name events are published under. Each event is routed to
/// `{base}.{context}` (context = the aggregate's first segment), so every
/// bounded context owns a stream: `sanvi.events.tenancy`, `sanvi.events.billing`, …
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

    /// Stream a record belongs to, derived from its aggregate type.
    pub fn stream_for(&self, aggregate: &str) -> String {
        let context = aggregate.split('.').next().unwrap_or("platform");
        format!("{}.{context}", self.0)
    }
}

/// Row shape of `platform.outbox`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub id: EventId,
    pub aggregate: String,
    pub aggregate_id: SnowflakeId,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
    pub attempts: i32,
}

#[derive(Debug, FromRow)]
struct OutboxRow {
    id: i64,
    aggregate: String,
    aggregate_id: i64,
    event_type: String,
    payload: serde_json::Value,
    metadata: serde_json::Value,
    occurred_at: DateTime<Utc>,
    published_at: Option<DateTime<Utc>>,
    attempts: i32,
}

impl TryFrom<OutboxRow> for OutboxRecord {
    type Error = anyhow::Error;

    fn try_from(row: OutboxRow) -> Result<Self> {
        Ok(Self {
            id: EventId::try_from(row.id)
                .map_err(|error| anyhow::anyhow!("invalid outbox event id: {error}"))?,
            aggregate: row.aggregate,
            aggregate_id: SnowflakeId::try_from(row.aggregate_id)
                .map_err(|error| anyhow::anyhow!("invalid outbox aggregate id: {error}"))?,
            event_type: row.event_type,
            payload: row.payload,
            metadata: row.metadata,
            occurred_at: row.occurred_at,
            published_at: row.published_at,
            attempts: row.attempts,
        })
    }
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
///   poll. A row that exhausts `max_attempts` moves to `platform.outbox_dlq`
///   so a poison message cannot block the queue forever.
#[derive(Clone)]
pub struct OutboxDispatcher {
    pool: PgPool,
    streams: StreamsClient,
    stream: StreamName,
    batch_size: usize,
    max_attempts: i32,
    metrics: Option<std::sync::Arc<OutboxMetrics>>,
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
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: std::sync::Arc<OutboxMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
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
        let rows = sqlx::query_as::<_, OutboxRow>(
            r#"SELECT id, aggregate, aggregate_id, event_type, payload, metadata,
                      occurred_at, published_at, attempts
               FROM platform.outbox
               WHERE published_at IS NULL
                 AND attempts < $1
               ORDER BY occurred_at
               LIMIT $2
               FOR UPDATE SKIP LOCKED"#,
        )
        .bind(self.max_attempts)
        .bind(self.batch_size as i64)
        .fetch_all(&mut *tx)
        .await
        .context("failed to claim outbox batch")?;
        let rows = rows
            .into_iter()
            .map(OutboxRecord::try_from)
            .collect::<Result<Vec<_>>>()?;
        Ok((tx, rows))
    }

    async fn mark_published(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: EventId,
    ) -> Result<()> {
        sqlx::query("UPDATE platform.outbox SET published_at = now() WHERE id = $1")
            .bind(id.as_i64())
            .execute(&mut **tx)
            .await
            .context("failed to mark outbox row published")?;
        Ok(())
    }

    async fn record_failure(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: EventId,
    ) -> Result<()> {
        sqlx::query("UPDATE platform.outbox SET attempts = attempts + 1 WHERE id = $1")
            .bind(id.as_i64())
            .execute(&mut **tx)
            .await
            .context("failed to record outbox failure")?;
        Ok(())
    }

    /// Move an exhausted row to the dead-letter table (same transaction).
    async fn dead_letter(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        record: &OutboxRecord,
        failure: &str,
    ) -> Result<()> {
        sqlx::query(
            "WITH moved AS (
               DELETE FROM platform.outbox WHERE id = $1
               RETURNING aggregate, aggregate_id, event_type, payload, metadata,
                         occurred_at, published_at, attempts
             )
             INSERT INTO platform.outbox_dlq
               (id, aggregate, aggregate_id, event_type, payload, metadata,
                occurred_at, published_at, attempts, failure)
             SELECT $1, aggregate, aggregate_id, event_type, payload, metadata,
                    occurred_at, published_at, attempts, $2
             FROM moved",
        )
        .bind(record.id.as_i64())
        .bind(failure)
        .execute(&mut **tx)
        .await
        .context("failed to dead-letter outbox row")?;
        Ok(())
    }

    /// Number of unpublished rows currently waiting (gauge source).
    pub async fn pending_count(&self) -> Result<i64> {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM platform.outbox WHERE published_at IS NULL")
                .fetch_one(&self.pool)
                .await
                .context("failed to count pending outbox rows")?;
        Ok(count)
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
            self.refresh_pending_gauge();
            return Ok(0);
        }

        let mut published = 0;
        for record in batch {
            let envelope = record.envelope();
            let payload =
                serde_json::to_string(&envelope).context("failed to serialize event envelope")?;
            let stream = self.stream.stream_for(&record.aggregate);

            let started = std::time::Instant::now();
            let outcome = self
                .streams
                .append(&stream, &[("event", payload.as_str())], 10_000)
                .await;
            if let Some(metrics) = &self.metrics {
                metrics
                    .publish_latency
                    .observe(started.elapsed().as_secs_f64());
            }

            match outcome {
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
                    let exhausted = record.attempts + 1 >= self.max_attempts;
                    error!(error = %err, event_id = %record.id, attempts = record.attempts + 1,
                        exhausted, "failed to publish event to stream");
                    if exhausted {
                        match Self::dead_letter(&mut tx, &record, &err.to_string()).await {
                            Ok(()) => {
                                error!(event_id = %record.id, stream = %stream,
                                    "event dead-lettered after exhausting retries");
                                if let Some(metrics) = &self.metrics {
                                    metrics.dead_lettered.inc();
                                }
                            }
                            Err(dlq_err) => {
                                error!(error = %dlq_err, event_id = %record.id,
                                    "failed to dead-letter exhausted row; recording failure instead");
                                let _ = Self::record_failure(&mut tx, record.id).await;
                            }
                        }
                    } else if let Err(err) = Self::record_failure(&mut tx, record.id).await {
                        error!(error = %err, event_id = %record.id, "failed to record outbox failure");
                    }
                }
            }
        }
        tx.commit().await.context("failed to commit outbox batch")?;
        self.refresh_pending_gauge();
        Ok(published)
    }

    fn refresh_pending_gauge(&self) {
        if self.metrics.is_none() {
            return;
        }
        let metrics = self.metrics.clone().expect("checked above");
        let pool = self.pool.clone();
        tokio::spawn(async move {
            match sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM platform.outbox WHERE published_at IS NULL",
            )
            .fetch_one(&pool)
            .await
            {
                Ok(count) => metrics.pending.set(count),
                Err(err) => error!(error = %err, "failed to refresh outbox pending gauge"),
            }
        });
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
