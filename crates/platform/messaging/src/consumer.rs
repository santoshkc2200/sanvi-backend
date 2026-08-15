//! Redis Streams consumer with exactly-once processing semantics.
//!
//! - Reads as part of a consumer group; every handler run is guarded by a
//!   `(consumer_group, event_id)` dedup key in `platform.processed_events`,
//!   so at-least-once delivery (Redis semantics) collapses to exactly-once
//!   *processing*.
//! - Failed handlers get the event acked out of the group and forwarded to
//!   the dead-letter stream (`{stream}.dlq`) — poison messages can never
//!   block the group forever.
//! - Periodically reclaims entries that have sat unacked for longer than
//!   [`RECOVERY_MIN_IDLE`] via `XAUTOCLAIM` (crash recovery). This scans the
//!   *whole group*, not just this consumer's own pending list: a consumer
//!   name is a fresh UUID per process start (see `apps/worker`), so a
//!   crashed/restarted process would never again see entries left pending
//!   under its old, now-nonexistent name if recovery only looked at "its
//!   own" pending list. `XAUTOCLAIM` reassigns stale entries — regardless of
//!   which (possibly dead) consumer originally owned them — to whichever
//!   live consumer asks for them.
//!
//!   The dedup key is only written *after* the handler succeeds (not before
//!   it runs), so a reclaimed entry whose handler never finished (the
//!   original consumer crashed mid-handler) is retried rather than skipped.
//!   A genuine duplicate delivery of an already-completed event still finds
//!   the dedup row and is skipped without re-running the handler.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use redis::AsyncCommands;
use redis::streams::{StreamAutoClaimOptions, StreamAutoClaimReply};
use sanvi_kernel::EventEnvelope;
use sqlx::PgPool;
use tracing::{error, info};

use crate::{ConsumerMetrics, StreamsClient};

/// Boxed handler future: consume an event, return whether it was handled
/// (`Ok(())`) or must be dead-lettered (`Err`).
pub type ConsumerHandler = Arc<
    dyn Fn(EventEnvelope) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// How often pending entries are re-scanned for crash recovery.
const RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
/// An entry is only reclaimed once it has sat unacked for this long — well
/// past any normal handler runtime, so we never steal work from a consumer
/// that is simply still busy.
const RECOVERY_MIN_IDLE: Duration = Duration::from_secs(60);
/// Poll cadence between empty reads. Blocking reads are deliberately not
/// used: `redis::aio::ConnectionManager` multiplexes commands over one
/// connection, and a BLOCK-ed XREADGROUP would stall every other command
/// (including the dispatcher sharing the manager) until the timeout.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// One consumer instance within a consumer group.
#[derive(Clone)]
pub struct StreamConsumer {
    pool: PgPool,
    streams: StreamsClient,
    metrics: Option<Arc<ConsumerMetrics>>,
    recovery_min_idle: Duration,
    recovery_interval: Duration,
}

impl StreamConsumer {
    pub fn new(pool: PgPool, streams: StreamsClient) -> Self {
        Self {
            pool,
            streams,
            metrics: None,
            recovery_min_idle: RECOVERY_MIN_IDLE,
            recovery_interval: RECOVERY_INTERVAL,
        }
    }

    pub fn with_metrics(mut self, metrics: Arc<ConsumerMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Override how long an entry must sit unacked before `recover_pending`
    /// reclaims it, and how often that reclaim runs. Production uses
    /// [`RECOVERY_MIN_IDLE`]/[`RECOVERY_INTERVAL`]; tests use much shorter
    /// values so crash-recovery can be exercised without a real minute-long
    /// wait.
    pub fn with_recovery_tuning(mut self, min_idle: Duration, interval: Duration) -> Self {
        self.recovery_min_idle = min_idle;
        self.recovery_interval = interval;
        self
    }

    /// Run until `shutdown` fires. Exits after the current batch.
    pub async fn run_until_shutdown(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
        handler: ConsumerHandler,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        info!(
            stream,
            group,
            consumer = consumer_name,
            "stream consumer started"
        );
        if let Err(err) = self.streams.ensure_consumer_group(stream, group).await {
            error!(error = %err, stream, group, "failed to ensure consumer group; stopping");
            return;
        }

        let mut last_recovery = tokio::time::Instant::now();
        loop {
            // Poll for a batch, but abandon immediately on shutdown.
            let batch = tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
                result = self.read(stream, group, consumer_name) => match result {
                    Ok(batch) => batch,
                    Err(err) => {
                        error!(error = %err, stream, group, "stream read failed");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                },
            };

            let batch_was_empty = batch.is_empty();
            for entry in batch {
                self.process_one(stream, group, &entry.0, &entry.1, &handler)
                    .await;
            }

            // Crash recovery: reclaim entries idle longer than
            // recovery_min_idle across the whole group (not just entries
            // this consumer name happens to own) periodically. Runs on this
            // timer regardless of whether the poll above found anything —
            // a quiet stream must not stall recovery of a stale entry left
            // by a consumer that crashed mid-handler.
            if last_recovery.elapsed() >= self.recovery_interval {
                if let Err(err) = self
                    .recover_pending(stream, group, consumer_name, &handler)
                    .await
                {
                    error!(error = %err, stream, group, "pending recovery failed");
                }
                last_recovery = tokio::time::Instant::now();
            }

            if let Some(metrics) = &self.metrics
                && let Ok(lag) = self.pending_count(stream, group).await
            {
                metrics.lag.set(lag as i64);
            }

            if batch_was_empty {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
        info!(stream, group, "stream consumer stopped");
    }

    async fn read(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut conn = self.streams.connection_manager();
        let options = redis::streams::StreamReadOptions::default()
            .group(group, consumer_name)
            .count(32);
        let reply: redis::streams::StreamReadReply = conn
            .xread_options(&[stream], &[">"], &options)
            .await
            .with_context(|| format!("failed to read stream {stream}"))?;

        let mut entries = Vec::new();
        for key in reply.keys {
            for entry in key.ids {
                let event = entry
                    .map
                    .get("event")
                    .cloned()
                    .map(|value| value_as_string(&value))
                    .unwrap_or_default();
                entries.push((entry.id, event));
            }
        }
        Ok(entries)
    }

    /// Reclaim entries that have sat unacked for longer than
    /// [`RECOVERY_MIN_IDLE`], regardless of which consumer originally
    /// claimed them, and (re)process them under `consumer_name`.
    ///
    /// Uses `XAUTOCLAIM` rather than re-reading `consumer_name`'s own
    /// pending list: consumer names are ephemeral (a fresh UUID per process
    /// start), so a crashed process's stale entries sit under a name that
    /// will never read its own pending list again. `XAUTOCLAIM` scans the
    /// whole group and reassigns ownership to whoever calls it.
    async fn recover_pending(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
        handler: &ConsumerHandler,
    ) -> Result<()> {
        let mut conn = self.streams.connection_manager();
        let min_idle_ms = self.recovery_min_idle.as_millis() as usize;
        let mut cursor = "0-0".to_string();

        loop {
            let reply: StreamAutoClaimReply = conn
                .xautoclaim_options(
                    stream,
                    group,
                    consumer_name,
                    min_idle_ms,
                    cursor.clone(),
                    StreamAutoClaimOptions::default().count(32),
                )
                .await
                .with_context(|| format!("failed to autoclaim pending entries on {stream}"))?;

            for entry in &reply.claimed {
                let event = entry
                    .map
                    .get("event")
                    .cloned()
                    .map(|value| value_as_string(&value))
                    .unwrap_or_default();
                self.process_one(stream, group, &entry.id, &event, handler)
                    .await;
            }

            // Entries Redis dropped from the stream (e.g. trimmed) while
            // still pending can never be delivered again; nothing to do but
            // note it so an operator can investigate data loss upstream.
            if !reply.deleted_ids.is_empty() {
                error!(
                    stream,
                    group,
                    count = reply.deleted_ids.len(),
                    "autoclaim found pending entries trimmed from the stream"
                );
            }

            if reply.claimed.is_empty() || reply.next_stream_id == "0-0" {
                break;
            }
            cursor = reply.next_stream_id;
        }
        Ok(())
    }

    async fn process_one(
        &self,
        stream: &str,
        group: &str,
        entry_id: &str,
        event_json: &str,
        handler: &ConsumerHandler,
    ) {
        let envelope: EventEnvelope = match serde_json::from_str(event_json) {
            Ok(envelope) => envelope,
            Err(err) => {
                // Unparseable payloads are poison by definition: ack them out
                // of the group so they cannot block delivery forever.
                error!(error = %err, entry_id, stream, group, "unparseable event payload");
                if let Err(err) = self.ack(stream, group, entry_id).await {
                    error!(error = %err, entry_id, "failed to ack unparseable event");
                }
                return;
            }
        };

        // Dedup keyed per consumer group so two different groups may process
        // the same event. Checked *before* the handler runs and written
        // *after* it succeeds (not the other way around): if this process
        // dies mid-handler, no dedup row exists, so a reclaimed redelivery
        // (see `recover_pending`) retries the handler instead of finding a
        // stale claim and silently skipping it.
        let already_done = match sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM platform.processed_events WHERE consumer = $1 AND event_id = $2)",
        )
        .bind(group)
        .bind(envelope.id)
        .fetch_one(&self.pool)
        .await
        {
            Ok(exists) => exists,
            Err(err) => {
                error!(error = %err, event_id = %envelope.id, group, "dedup check failed; skipping");
                return;
            }
        };
        if already_done {
            // Already fully processed by this group: safe to forget.
            if let Err(err) = self.ack(stream, group, entry_id).await {
                error!(error = %err, entry_id, "failed to ack duplicate event");
            }
            return;
        }

        match handler(envelope.clone()).await {
            Ok(()) => {
                if let Err(err) = sqlx::query(
                    "INSERT INTO platform.processed_events (consumer, event_id)
                     VALUES ($1, $2)
                     ON CONFLICT DO NOTHING",
                )
                .bind(group)
                .bind(envelope.id)
                .execute(&self.pool)
                .await
                {
                    error!(error = %err, event_id = %envelope.id, group, "failed to record processed event");
                }
                if let Err(err) = self.ack(stream, group, entry_id).await {
                    error!(error = %err, entry_id, "failed to ack processed event");
                }
            }
            Err(err) => {
                error!(error = %err, event_id = %envelope.id, event_type = %envelope.event_type,
                    "handler failed; dead-lettering event");
                let payload = serde_json::to_string(&envelope).unwrap_or_default();
                let dlq = format!("{stream}.dlq");
                if let Err(dlq_err) = self
                    .streams
                    .append(
                        &dlq,
                        &[("event", payload.as_str()), ("error", &err.to_string())],
                        10_000,
                    )
                    .await
                {
                    error!(error = %dlq_err, stream = %dlq, "failed to append to dead-letter stream");
                }
                if let Err(ack_err) = self.ack(stream, group, entry_id).await {
                    error!(error = %ack_err, entry_id, "failed to ack dead-lettered event");
                }
                if let Some(metrics) = &self.metrics {
                    metrics.dead_lettered.inc();
                }
            }
        }
    }

    async fn ack(&self, stream: &str, group: &str, entry_id: &str) -> Result<()> {
        let mut conn = self.streams.connection_manager();
        let _: () = conn
            .xack(stream, group, &[entry_id])
            .await
            .with_context(|| format!("failed to ack entry {entry_id} on {stream}"))?;
        Ok(())
    }

    async fn pending_count(&self, stream: &str, group: &str) -> Result<u64> {
        let mut conn = self.streams.connection_manager();
        let reply: redis::streams::StreamPendingReply =
            conn.xpending(stream, group).await.with_context(|| {
                format!("failed to read pending count for group {group} on {stream}")
            })?;
        Ok(reply.count() as u64)
    }
}

fn value_as_string(value: &redis::Value) -> String {
    match value {
        redis::Value::BulkString(bytes) => String::from_utf8_lossy(bytes).to_string(),
        other => format!("{other:?}"),
    }
}
