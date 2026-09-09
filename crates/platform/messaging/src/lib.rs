//! Sanvi messaging: Redis Streams client, the transactional outbox (writer +
//! dispatcher), stream consumers and the leader-elected scheduler.
//!
//! Contexts write domain events to `platform.outbox` in the same transaction
//! as their state change. [`OutboxDispatcher`] (running in `apps/worker`)
//! relays unpublished events to per-context Redis Streams
//! (`sanvi.events.{context}`); [`StreamConsumer`]s read from streams as
//! consumer groups and deduplicate on the event id, with dead-lettering for
//! poison messages. [`Scheduler`] runs periodic jobs on a leader-elected
//! worker.

mod config;
mod consumer;
mod metrics;
mod outbox;
mod redis;
mod scheduler;
mod writer;

pub use config::RedisConfig;
pub use consumer::{
    ConsumerHandler, HandlerError, StreamConsumer, TransactionalConsumerHandler, is_transient_error,
};
pub use metrics::{ConsumerMetrics, OutboxMetrics};
pub use outbox::{OutboxDispatcher, OutboxRecord, StreamName};
pub use redis::{StreamsClient, connect_redis};
pub use scheduler::{FnJob, Job, Scheduler};
pub use writer::OutboxWriter;
