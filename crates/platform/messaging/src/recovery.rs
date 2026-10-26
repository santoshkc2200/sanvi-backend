//! Retained PostgreSQL outbox events support primary delivery and broker recovery.
//!
//! Each consumer group keeps a cursor in `platform.consumer_cursors`. Rows are
//! read in publication-sequence order. Dispatchers publish under one advisory
//! lock, so sequence order is commit order and no row can later appear behind
//! a cursor.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use sqlx::PgPool;

use crate::outbox::{OutboxRecord, OutboxRow, PUBLICATION_LOCK_KEY};

pub(crate) const RECOVERY_BATCH: usize = 32;

/// A delivery position: publication sequence, then event id. Rows published
/// before sequencing existed sit at sequence 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct Position {
    published_seq: i64,
    event_id: i64,
}

impl Position {
    /// The position just before this one. Everything at or below it precedes
    /// this event in delivery order.
    pub(crate) fn before(self) -> Self {
        Self {
            event_id: self.event_id - 1,
            ..self
        }
    }
}

#[derive(sqlx::FromRow)]
struct PendingRow {
    #[sqlx(flatten)]
    row: OutboxRow,
    published_seq: i64,
}

fn context_of(stream: &str) -> &str {
    stream.rsplit('.').next().unwrap_or(stream)
}

/// The group's cursor, created on first use. A tail-only group starts after
/// the last assigned sequence. Holding the publication lock while reading it
/// means no publication with a lower sequence is still uncommitted.
async fn cursor(pool: &PgPool, stream: &str, group: &str, tail: bool) -> Result<Position> {
    let select = || {
        sqlx::query_as::<_, Position>(
            "SELECT published_seq, event_id FROM platform.consumer_cursors
             WHERE consumer = $1 AND stream = $2",
        )
        .bind(group)
        .bind(stream)
        .fetch_optional(pool)
    };
    if let Some(found) = select().await.context("failed to read consumer cursor")? {
        return Ok(found);
    }
    let mut tx = pool.begin().await?;
    if tail {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(PUBLICATION_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .context("failed to wait for in-flight publication")?;
    }
    sqlx::query(
        "INSERT INTO platform.consumer_cursors (consumer, stream, published_seq, event_id)
         SELECT $1, $2, CASE WHEN is_called THEN last_value ELSE 0 END, $4
         FROM platform.outbox_publication_seq WHERE $3
         UNION ALL SELECT $1, $2, 0, 0 WHERE NOT $3
         ON CONFLICT DO NOTHING",
    )
    .bind(group)
    .bind(stream)
    .bind(tail)
    .bind(i64::MAX)
    .execute(&mut *tx)
    .await
    .context("failed to create consumer cursor")?;
    tx.commit().await?;
    select()
        .await?
        .context("consumer cursor missing after creation")
}

/// Move the cursor forward; it never moves back, so concurrent callers are safe.
pub(crate) async fn advance(pool: &PgPool, stream: &str, group: &str, to: Position) -> Result<()> {
    sqlx::query(
        "UPDATE platform.consumer_cursors SET published_seq = $3, event_id = $4
         WHERE consumer = $1 AND stream = $2 AND (published_seq, event_id) < ($3, $4)",
    )
    .bind(group)
    .bind(stream)
    .bind(to.published_seq)
    .bind(to.event_id)
    .execute(pool)
    .await
    .context("failed to advance consumer cursor")?;
    Ok(())
}

pub(crate) async fn pending(
    pool: &PgPool,
    stream: &str,
    group: &str,
    tail: bool,
) -> Result<Vec<(Position, EventEnvelope)>> {
    let from = cursor(pool, stream, group, tail).await?;
    let rows: Vec<PendingRow> = sqlx::query_as(
        "SELECT o.id, o.aggregate, o.aggregate_id, o.event_type, o.payload,
                o.metadata, o.occurred_at, o.published_at, o.attempts,
                COALESCE(o.published_seq, 0) AS published_seq
         FROM platform.outbox o
         WHERE split_part(o.aggregate, '.', 1) = $1 AND o.published_at IS NOT NULL
           AND (COALESCE(o.published_seq, 0), o.id) > ($3, $4)
           AND NOT EXISTS (SELECT 1 FROM platform.processed_events p
                           WHERE p.consumer = $2 AND p.event_id = o.id)
           AND NOT EXISTS (SELECT 1 FROM platform.consumer_dead_letters d
                           WHERE d.consumer = $2 AND d.event_id = o.id)
         ORDER BY COALESCE(o.published_seq, 0), o.id LIMIT $5",
    )
    .bind(context_of(stream))
    .bind(group)
    .bind(from.published_seq)
    .bind(from.event_id)
    .bind(RECOVERY_BATCH as i64)
    .fetch_all(pool)
    .await
    .context("failed to recover retained outbox events")?;
    rows.into_iter()
        .map(|pending| {
            let position = Position {
                published_seq: pending.published_seq,
                event_id: pending.row.id(),
            };
            Ok((position, OutboxRecord::try_from(pending.row)?.envelope()))
        })
        .collect()
}

/// Published events past the group's cursor that it has not yet settled.
pub(crate) async fn pending_count(
    pool: &PgPool,
    stream: &str,
    group: &str,
    tail: bool,
) -> Result<i64> {
    let from = cursor(pool, stream, group, tail).await?;
    sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox o
         WHERE split_part(o.aggregate, '.', 1) = $1 AND o.published_at IS NOT NULL
           AND (COALESCE(o.published_seq, 0), o.id) > ($3, $4)
           AND NOT EXISTS (SELECT 1 FROM platform.processed_events p
                           WHERE p.consumer = $2 AND p.event_id = o.id)
           AND NOT EXISTS (SELECT 1 FROM platform.consumer_dead_letters d
                           WHERE d.consumer = $2 AND d.event_id = o.id)",
    )
    .bind(context_of(stream))
    .bind(group)
    .bind(from.published_seq)
    .bind(from.event_id)
    .fetch_one(pool)
    .await
    .context("failed to count pending outbox events")
}

pub(crate) async fn park(
    pool: &PgPool,
    stream: &str,
    group: &str,
    entry: &str,
    event_id: Option<i64>,
    payload: &str,
    failure: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO platform.consumer_dead_letters
           (stream, consumer, entry_id, event_id, event_payload, failure)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT DO NOTHING",
    )
    .bind(stream)
    .bind(group)
    .bind(entry)
    .bind(event_id)
    .bind(payload)
    .bind(failure)
    .execute(pool)
    .await
    .context("failed to durably park consumer event")?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
pub struct ConsumerDeadLetter {
    pub stream: String,
    pub consumer: String,
    pub entry_id: String,
    pub event_id: Option<i64>,
    pub failure: String,
    pub dead_lettered_at: DateTime<Utc>,
}

pub async fn list_consumer_dead_letters(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ConsumerDeadLetter>> {
    sqlx::query_as(
        "SELECT stream, consumer, entry_id, event_id, failure, dead_lettered_at
         FROM platform.consumer_dead_letters
         ORDER BY dead_lettered_at DESC, consumer, stream, entry_id LIMIT $1",
    )
    .bind(limit.clamp(1, 1000))
    .fetch_all(pool)
    .await
    .context("failed to list consumer failures")
}

/// Audited operator replay: persist the envelope back to the outbox before
/// removing its parked marker. Other groups deduplicate the repeated event.
pub async fn replay_consumer_dead_letter(
    scope: &mut sanvi_persistence::PlatformScope<'_>,
    group: &str,
    event_id: i64,
) -> Result<bool> {
    let tx = scope.tx_mut().as_inner();
    let payload: Option<String> = sqlx::query_scalar(
        "DELETE FROM platform.consumer_dead_letters WHERE consumer = $1 AND event_id = $2 RETURNING event_payload",
    ).bind(group).bind(event_id).fetch_optional(&mut **tx).await?;
    let Some(payload) = payload else {
        return Ok(false);
    };
    let event: EventEnvelope =
        serde_json::from_str(&payload).context("parked event is not replayable")?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM platform.outbox WHERE id = $1)")
            .bind(event_id)
            .fetch_one(&mut **tx)
            .await?;
    if exists {
        sqlx::query("UPDATE platform.outbox SET published_at = NULL, attempts = 0 WHERE id = $1")
            .bind(event_id)
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload, metadata, occurred_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(event.id.as_i64()).bind(&event.aggregate).bind(event.aggregate_id.as_i64())
            .bind(&event.event_type).bind(&event.payload).bind(&event.metadata).bind(event.occurred_at)
            .execute(&mut **tx).await?;
    }
    Ok(true)
}
