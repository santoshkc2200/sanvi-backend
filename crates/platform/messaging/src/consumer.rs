//! Event consumer with PostgreSQL delivery and an optional Redis Streams adapter.
//!
//! [`StreamConsumer::postgres`] polls retained published outbox events with a
//! renewable PostgreSQL lease per group. Inbox records and durable dead letters
//! acknowledge work without Redis. Interrupted batches replay safely; external
//! effects must be idempotent. [`StreamConsumer::new`] selects the Redis adapter.
//! The Redis delivery and recovery details below apply to that adapter.
//!
//! - **Guarantees**:
//!   Redis Streams provides at-least-once delivery semantics. To ensure correctness:
//!   - For handlers whose effects are database writes, [`StreamConsumer::run_transactional_until_shutdown`]
//!     commits the inbox deduplication record in `platform.processed_events` and the domain business
//!     mutations inside the same PostgreSQL transaction. A crash before commit leaves no trace of the
//!     effect and no dedup row, allowing clean replay upon redelivery without duplicate side effects.
//!   - For handlers with external side effects (e.g. caches, third-party network APIs), transactional
//!     atomicity across system boundaries is impossible. Handlers executed via
//!     [`StreamConsumer::run_until_shutdown`] MUST be idempotent and support reconciliation rather
//!     than assuming exactly-once execution.
//!
//! - **Failure handling and durability**:
//!   - Handler errors are classified as transient or permanent via [`HandlerError`].
//!   - Transient errors (e.g. connection pool exhaustion, database serialization/deadlock conflicts)
//!     are retried with exponential backoff up to a configured limit (`max_retries`).
//!   - Permanent errors (or exhausted retries) are forwarded to the dead-letter stream (`{stream}.dlq`).
//!   - Crucially, the original entry is **never acknowledged (`XACK`) unless the outcome is durable**:
//!     if appending to the dead-letter stream fails, the consumer refuses to ack the entry so that the
//!     message remains pending in the group and is not lost.
//!
//! - **Crash recovery**:
//!   Periodically reclaims entries that have sat unacked for longer than [`RECOVERY_MIN_IDLE`] via
//!   `XAUTOCLAIM`. This scans the whole consumer group, reassigning stale unacked entries from crashed
//!   consumers to live consumers.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use redis::AsyncCommands;
use redis::streams::{StreamAutoClaimOptions, StreamAutoClaimReply};
use sanvi_kernel::EventEnvelope;
use sqlx::PgPool;
use tracing::{error, info};

use crate::{ConsumerMetrics, StreamsClient};

/// Classification of a consumer handler error.
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    /// A transient failure (e.g. network glitch, lock contention, timeout).
    /// The consumer will retry this event with exponential backoff.
    #[error("transient error: {0}")]
    Transient(#[source] anyhow::Error),

    /// A permanent failure (e.g. invalid payload, constraint violation).
    /// The consumer will immediately dead-letter this event without retrying.
    #[error("permanent error: {0}")]
    Permanent(#[source] anyhow::Error),
}

impl HandlerError {
    pub fn transient(err: impl Into<anyhow::Error>) -> Self {
        Self::Transient(err.into())
    }

    pub fn permanent(err: impl Into<anyhow::Error>) -> Self {
        Self::Permanent(err.into())
    }

    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Checks whether an error represents a transient failure eligible for retry.
pub fn is_transient_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(he) = cause.downcast_ref::<HandlerError>() {
            return he.is_transient();
        }
        if let Some(sqlx_err) = cause.downcast_ref::<sqlx::Error>() {
            return is_sqlx_transient(sqlx_err);
        }
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            return is_io_transient(io_err);
        }
    }
    false
}

fn is_sqlx_transient(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => true,
        sqlx::Error::Database(db_err) => {
            let code = db_err.code().unwrap_or_default();
            // 40001: serialization_failure, 40P01: deadlock_detected, 08*: connection errors, 57P*: admin shutdown
            code == "40001" || code == "40P01" || code.starts_with("08") || code.starts_with("57P")
        }
        _ => false,
    }
}

fn is_io_transient(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::TimedOut
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::WouldBlock
    )
}

/// Boxed handler future for external/non-transactional handlers.
///
/// External effects cannot participate in a database transaction; handlers
/// must be idempotent and support reconciliation upon redelivery.
pub type ConsumerHandler = Arc<
    dyn Fn(EventEnvelope) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// Boxed handler future for transactional database handlers.
///
/// The handler executes within an active PostgreSQL transaction that also commits
/// the inbox dedup row (`platform.processed_events`).
pub type TransactionalConsumerHandler = Arc<
    dyn for<'c> Fn(
            &'c mut sqlx::Transaction<'_, sqlx::Postgres>,
            EventEnvelope,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'c>>
        + Send
        + Sync
        + 'static,
>;

#[derive(Clone)]
enum ConsumerHandlerKind {
    External(ConsumerHandler),
    Transactional(TransactionalConsumerHandler),
}

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

/// Default retry settings for transient handler failures.
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(50);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(2);

/// One consumer instance within a durable consumer group.
#[derive(Clone)]
pub struct StreamConsumer {
    pool: PgPool,
    streams: Option<StreamsClient>,
    transactions: Option<sanvi_persistence::TxManager>,
    metrics: Option<Arc<ConsumerMetrics>>,
    recovery_min_idle: Duration,
    recovery_interval: Duration,
    max_retries: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl StreamConsumer {
    pub fn new(pool: PgPool, streams: StreamsClient) -> Self {
        Self::with_transport(pool, Some(streams))
    }

    /// Consume retained outbox rows using PostgreSQL coordination, inbox and DLQ.
    pub fn postgres(transactions: sanvi_persistence::TxManager) -> Self {
        let mut consumer = Self::with_transport(transactions.pool().clone(), None);
        consumer.transactions = Some(transactions);
        consumer
    }

    fn with_transport(pool: PgPool, streams: Option<StreamsClient>) -> Self {
        Self {
            pool,
            streams,
            transactions: None,
            metrics: None,
            recovery_min_idle: RECOVERY_MIN_IDLE,
            recovery_interval: RECOVERY_INTERVAL,
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
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

    /// Override retry policy for transient failures.
    pub fn with_retry_tuning(
        mut self,
        max_retries: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        self.max_retries = max_retries;
        self.initial_backoff = initial_backoff;
        self.max_backoff = max_backoff;
        self
    }

    fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        let mult = 2u32.saturating_pow(attempt.saturating_sub(1));
        let duration = self.initial_backoff.saturating_mul(mult);
        duration.min(self.max_backoff)
    }

    /// Run until `shutdown` fires using an external-effects handler. Exits after the current batch.
    pub async fn run_until_shutdown(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
        handler: ConsumerHandler,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        self.run_until_shutdown_at(stream, group, consumer_name, handler, shutdown, "0")
            .await;
    }

    /// Run until `shutdown` fires, ensuring the consumer group starts at `start_id` (e.g. `"$"` for tail-only).
    pub async fn run_until_shutdown_at(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
        handler: ConsumerHandler,
        shutdown: tokio::sync::watch::Receiver<bool>,
        start_id: &str,
    ) {
        self.run_loop_at(
            stream,
            group,
            consumer_name,
            ConsumerHandlerKind::External(handler),
            shutdown,
            start_id,
        )
        .await;
    }

    /// Run until `shutdown` fires using a transactional database handler. Exits after the current batch.
    ///
    /// The handler executes inside an active PostgreSQL transaction alongside the
    /// `platform.processed_events` deduplication insert, achieving atomic commit of both.
    pub async fn run_transactional_until_shutdown(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
        handler: TransactionalConsumerHandler,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        self.run_loop_at(
            stream,
            group,
            consumer_name,
            ConsumerHandlerKind::Transactional(handler),
            shutdown,
            "0",
        )
        .await;
    }

    async fn run_loop_at(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
        handler: ConsumerHandlerKind,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        start_id: &str,
    ) {
        let Some(streams) = &self.streams else {
            self.run_postgres_loop(stream, group, consumer_name, handler, shutdown, start_id)
                .await;
            return;
        };
        info!(
            stream,
            group,
            consumer = consumer_name,
            start_id,
            "stream consumer started"
        );
        if let Err(err) = streams
            .ensure_consumer_group_at(stream, group, start_id)
            .await
        {
            error!(error = %err, stream, group, start_id, "failed to ensure consumer group; stopping");
            return;
        }

        let mut last_recovery = tokio::time::Instant::now();
        let mut next_outbox_recovery = tokio::time::Instant::now();
        let recovery_since = (start_id == "$").then(chrono::Utc::now);
        loop {
            if *shutdown.borrow() {
                break;
            }
            if tokio::time::Instant::now() >= next_outbox_recovery {
                let recovered = self
                    .recover_outbox(stream, group, &handler, recovery_since)
                    .await;
                next_outbox_recovery = tokio::time::Instant::now()
                    + match recovered {
                        Ok(count) if count == crate::recovery::RECOVERY_BATCH => POLL_INTERVAL,
                        Ok(_) => self.recovery_interval,
                        Err(err) => {
                            error!(error = %err, stream, group, "PostgreSQL event recovery failed");
                            self.recovery_interval
                        }
                    };
            }
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
                        // A Redis restart or eviction may remove the group.
                        // Re-create it and let PostgreSQL fill any missing tail.
                        if let Err(err) = streams.ensure_consumer_group_at(stream, group, "0").await {
                            error!(error = %err, "failed to recreate consumer group");
                        }
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
            // recovery_min_idle across the whole group periodically.
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

    async fn run_postgres_loop(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
        handler: ConsumerHandlerKind,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        start_id: &str,
    ) {
        let Some(transactions) = &self.transactions else {
            return;
        };
        let lease = crate::lease::PgLease::new(
            transactions.clone(),
            format!("consumer:{group}:{stream}"),
            consumer_name.into(),
            Duration::from_secs(60),
        );
        // Worker subscriptions replay retained history. Tail-only subscriptions must
        // retain their original starting time through restarts and leader changes.
        let mut since = None;
        let mut initialized = start_id != "$";
        info!(stream, group, "PostgreSQL event consumer started");
        while !*shutdown.borrow() && shutdown.has_changed().is_ok() {
            let acquired: Result<bool> = async {
                if !lease.acquire_or_renew().await? {
                    return Ok(false);
                }
                if !initialized {
                    since = Some(
                        sqlx::query_scalar!(
                            "SELECT started_at FROM platform.worker_leases WHERE name = $1",
                            lease.name,
                        )
                        .fetch_one(&self.pool)
                        .await?,
                    );
                    initialized = true;
                }
                Ok(true)
            }
            .await;
            match acquired {
                Ok(true) => {
                    let batch = self.recover_outbox(stream, group, &handler, since);
                    match lease.protect(batch, &mut shutdown).await {
                        Ok(Some(count)) => {
                            if let Some(metrics) = &self.metrics
                                && let Ok(pending) =
                                    crate::recovery::pending_count(&self.pool, stream, group, since)
                                        .await
                            {
                                metrics.lag.set(pending);
                            }
                            if count == crate::recovery::RECOVERY_BATCH {
                                continue;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            error!(error = %err, stream, group, "PostgreSQL event delivery interrupted")
                        }
                    }
                }
                Ok(false) => {}
                Err(err) => error!(error = %err, stream, group, "PostgreSQL consumer lease failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {},
                _ = shutdown.changed() => break,
            }
        }
        if let Err(err) = lease.release().await {
            error!(error = %err, stream, group, "failed to release consumer lease");
        }
        info!(stream, group, "PostgreSQL event consumer stopped");
    }

    async fn recover_outbox(
        &self,
        stream: &str,
        group: &str,
        handler: &ConsumerHandlerKind,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<usize> {
        let events = crate::recovery::pending(&self.pool, stream, group, since).await?;
        let count = events.len();
        for envelope in events {
            let payload = serde_json::to_string(&envelope)?;
            // Valid Redis ID outside the timestamp range of ordinary XADD IDs.
            let entry = format!("0-{}", envelope.id.as_i64());
            self.process_one(stream, group, &entry, &payload, handler)
                .await;
        }
        Ok(count)
    }

    async fn read(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut conn = self
            .streams
            .as_ref()
            .context("Redis transport is not configured")?
            .connection_manager();
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
    async fn recover_pending(
        &self,
        stream: &str,
        group: &str,
        consumer_name: &str,
        handler: &ConsumerHandlerKind,
    ) -> Result<()> {
        let mut conn = self
            .streams
            .as_ref()
            .context("Redis transport is not configured")?
            .connection_manager();
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
        handler_kind: &ConsumerHandlerKind,
    ) {
        let envelope: EventEnvelope = match serde_json::from_str(event_json) {
            Ok(envelope) => envelope,
            Err(err) => {
                error!(error = %err, entry_id, stream, group, "unparseable event payload");
                let dlq = format!("{stream}.dlq");
                if let Err(park_err) = crate::recovery::park(
                    &self.pool,
                    stream,
                    group,
                    entry_id,
                    None,
                    event_json,
                    &err.to_string(),
                )
                .await
                {
                    error!(error = %park_err, "failed to park malformed event; refusing to ack");
                    return;
                }
                if let Err(dlq_err) = self
                    .append_dead_letter(
                        &dlq,
                        &[("raw_event", event_json), ("error", &err.to_string())],
                    )
                    .await
                {
                    error!(
                        error = %dlq_err,
                        stream = %dlq,
                        entry_id,
                        "failed to append unparseable event to dead-letter stream; refusing to ack"
                    );
                    return;
                }
                if let Err(ack_err) = self.ack(stream, group, entry_id).await {
                    error!(error = %ack_err, entry_id, "failed to ack unparseable event");
                }
                if let Some(metrics) = &self.metrics {
                    metrics.dead_lettered.inc();
                }
                return;
            }
        };

        let parked = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM platform.consumer_dead_letters WHERE consumer = $1 AND event_id = $2)",
        ).bind(group).bind(envelope.id.as_i64()).fetch_one(&self.pool).await;
        match parked {
            Ok(true) => {
                if let Err(err) = self.ack(stream, group, entry_id).await {
                    error!(error = %err, "failed to ack durably parked event");
                }
                return;
            }
            Err(err) => {
                error!(error = %err, "failed to inspect consumer recovery state; refusing to ack");
                return;
            }
            Ok(false) => {}
        }
        match handler_kind {
            ConsumerHandlerKind::Transactional(handler) => {
                self.process_transactional(stream, group, entry_id, &envelope, handler)
                    .await;
            }
            ConsumerHandlerKind::External(handler) => {
                self.process_external(stream, group, entry_id, &envelope, handler)
                    .await;
            }
        }
    }

    async fn process_transactional(
        &self,
        stream: &str,
        group: &str,
        entry_id: &str,
        envelope: &EventEnvelope,
        handler: &TransactionalConsumerHandler,
    ) {
        let mut attempt: u32 = 0;
        loop {
            let mut tx = match self.pool.begin().await {
                Ok(tx) => tx,
                Err(err) => {
                    let err = anyhow::Error::new(err);
                    if is_transient_error(&err) && attempt < self.max_retries {
                        attempt += 1;
                        let backoff = self.backoff_for_attempt(attempt);
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    self.dead_letter(stream, group, entry_id, envelope, &err)
                        .await;
                    return;
                }
            };

            // Atomically record inbox dedup row inside the transaction.
            // If already committed, ON CONFLICT DO NOTHING returns 0 rows.
            let inserted: Option<i64> = match sqlx::query_scalar::<_, i64>(
                "INSERT INTO platform.processed_events (consumer, event_id)
                 VALUES ($1, $2)
                 ON CONFLICT (consumer, event_id) DO NOTHING
                 RETURNING event_id",
            )
            .bind(group)
            .bind(envelope.id.as_i64())
            .fetch_optional(&mut *tx)
            .await
            {
                Ok(res) => res,
                Err(err) => {
                    let _ = tx.rollback().await;
                    let err = anyhow::Error::new(err);
                    if is_transient_error(&err) && attempt < self.max_retries {
                        attempt += 1;
                        let backoff = self.backoff_for_attempt(attempt);
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    self.dead_letter(stream, group, entry_id, envelope, &err)
                        .await;
                    return;
                }
            };

            if inserted.is_none() {
                // Event already processed by this group: drop tx and ack duplicate
                let _ = tx.rollback().await;
                if let Err(err) = self.ack(stream, group, entry_id).await {
                    error!(error = %err, entry_id, "failed to ack duplicate event");
                }
                return;
            }

            match handler(&mut tx, envelope.clone()).await {
                Ok(()) => {
                    if let Err(err) = tx.commit().await {
                        let err = anyhow::Error::new(err);
                        if is_transient_error(&err) && attempt < self.max_retries {
                            attempt += 1;
                            let backoff = self.backoff_for_attempt(attempt);
                            tokio::time::sleep(backoff).await;
                            continue;
                        }
                        self.dead_letter(stream, group, entry_id, envelope, &err)
                            .await;
                        return;
                    }

                    if let Err(err) = self.ack(stream, group, entry_id).await {
                        error!(error = %err, entry_id, "failed to ack processed event");
                    }
                    return;
                }
                Err(err) => {
                    let _ = tx.rollback().await;
                    if is_transient_error(&err) && attempt < self.max_retries {
                        attempt += 1;
                        let backoff = self.backoff_for_attempt(attempt);
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    self.dead_letter(stream, group, entry_id, envelope, &err)
                        .await;
                    return;
                }
            }
        }
    }

    async fn process_external(
        &self,
        stream: &str,
        group: &str,
        entry_id: &str,
        envelope: &EventEnvelope,
        handler: &ConsumerHandler,
    ) {
        let already_done = match sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM platform.processed_events WHERE consumer = $1 AND event_id = $2)",
        )
        .bind(group)
        .bind(envelope.id.as_i64())
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
            if let Err(err) = self.ack(stream, group, entry_id).await {
                error!(error = %err, entry_id, "failed to ack duplicate event");
            }
            return;
        }

        let mut attempt: u32 = 0;
        loop {
            match handler(envelope.clone()).await {
                Ok(()) => {
                    if let Err(err) = sqlx::query(
                        "INSERT INTO platform.processed_events (consumer, event_id)
                         VALUES ($1, $2)
                         ON CONFLICT DO NOTHING",
                    )
                    .bind(group)
                    .bind(envelope.id.as_i64())
                    .execute(&self.pool)
                    .await
                    {
                        error!(
                            error = %err,
                            event_id = %envelope.id,
                            group,
                            "failed to record processed event; refusing to ack so event will be retried"
                        );
                        return;
                    }
                    if let Err(err) = self.ack(stream, group, entry_id).await {
                        error!(error = %err, entry_id, "failed to ack processed event");
                    }
                    return;
                }
                Err(err) => {
                    if is_transient_error(&err) && attempt < self.max_retries {
                        attempt += 1;
                        let backoff = self.backoff_for_attempt(attempt);
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    self.dead_letter(stream, group, entry_id, envelope, &err)
                        .await;
                    return;
                }
            }
        }
    }

    async fn dead_letter(
        &self,
        stream: &str,
        group: &str,
        entry_id: &str,
        envelope: &EventEnvelope,
        err: &anyhow::Error,
    ) {
        error!(
            error = %err,
            event_id = %envelope.id,
            event_type = %envelope.event_type,
            group,
            "handler failed; dead-lettering event"
        );
        let payload = serde_json::to_string(envelope).unwrap_or_default();
        let dlq = format!("{stream}.dlq");
        if let Err(park_err) = crate::recovery::park(
            &self.pool,
            stream,
            group,
            entry_id,
            Some(envelope.id.as_i64()),
            &payload,
            &err.to_string(),
        )
        .await
        {
            error!(error = %park_err, "failed to durably park event; refusing to ack");
            return;
        }
        if let Err(dlq_err) = self
            .append_dead_letter(
                &dlq,
                &[("event", payload.as_str()), ("error", &err.to_string())],
            )
            .await
        {
            error!(
                error = %dlq_err,
                stream = %dlq,
                entry_id,
                "failed to append to dead-letter stream; refusing to ack so message is not lost"
            );
            return;
        }
        if let Err(ack_err) = self.ack(stream, group, entry_id).await {
            error!(error = %ack_err, entry_id, "failed to ack dead-lettered event");
        }
        if let Some(metrics) = &self.metrics {
            metrics.dead_lettered.inc();
        }
    }

    async fn append_dead_letter(&self, stream: &str, fields: &[(&str, &str)]) -> Result<()> {
        if let Some(streams) = &self.streams {
            streams.append_durable(stream, fields).await?;
        }
        Ok(())
    }

    async fn ack(&self, stream: &str, group: &str, entry_id: &str) -> Result<()> {
        if self.streams.is_none() {
            // The inbox or durable dead-letter record is PostgreSQL's acknowledgement.
            return Ok(());
        }
        let mut conn = self
            .streams
            .as_ref()
            .context("Redis transport is not configured")?
            .connection_manager();
        let _: () = conn
            .xack(stream, group, &[entry_id])
            .await
            .with_context(|| format!("failed to ack entry {entry_id} on {stream}"))?;
        Ok(())
    }

    async fn pending_count(&self, stream: &str, group: &str) -> Result<u64> {
        let mut conn = self
            .streams
            .as_ref()
            .context("Redis transport is not configured")?
            .connection_manager();
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
