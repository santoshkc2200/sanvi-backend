//! Retained PostgreSQL outbox events support primary delivery and broker recovery.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use sqlx::PgPool;

use crate::outbox::{OutboxRecord, OutboxRow};

pub(crate) const RECOVERY_BATCH: usize = 32;

pub(crate) async fn pending(
    pool: &PgPool,
    stream: &str,
    group: &str,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<EventEnvelope>> {
    let context = stream.rsplit('.').next().unwrap_or(stream);
    let rows: Vec<OutboxRow> = sqlx::query_as(
        "SELECT o.id, o.aggregate, o.aggregate_id, o.event_type, o.payload,
                o.metadata, o.occurred_at, o.published_at, o.attempts
         FROM platform.outbox o
         WHERE split_part(o.aggregate, '.', 1) = $1 AND o.published_at IS NOT NULL
           AND ($3::timestamptz IS NULL OR o.published_at >= $3)
           AND NOT EXISTS (SELECT 1 FROM platform.processed_events p
                           WHERE p.consumer = $2 AND p.event_id = o.id)
           AND NOT EXISTS (SELECT 1 FROM platform.consumer_dead_letters d
                           WHERE d.consumer = $2 AND d.event_id = o.id)
         ORDER BY o.occurred_at, o.id LIMIT $4",
    )
    .bind(context)
    .bind(group)
    .bind(since)
    .bind(RECOVERY_BATCH as i64)
    .fetch_all(pool)
    .await
    .context("failed to recover retained outbox events")?;
    rows.into_iter()
        .map(|row| Ok(OutboxRecord::try_from(row)?.envelope()))
        .collect()
}

pub(crate) async fn pending_count(
    pool: &PgPool,
    stream: &str,
    group: &str,
    since: Option<DateTime<Utc>>,
) -> Result<i64> {
    let context = stream.rsplit('.').next().unwrap_or(stream);
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM platform.outbox o
           WHERE split_part(o.aggregate, '.', 1) = $1 AND o.published_at IS NOT NULL
             AND ($3::timestamptz IS NULL OR o.published_at >= $3)
             AND NOT EXISTS (SELECT 1 FROM platform.processed_events p
                             WHERE p.consumer = $2 AND p.event_id = o.id)
             AND NOT EXISTS (SELECT 1 FROM platform.consumer_dead_letters d
                             WHERE d.consumer = $2 AND d.event_id = o.id)"#,
        context,
        group,
        since,
    )
    .fetch_one(pool)
    .await?)
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
