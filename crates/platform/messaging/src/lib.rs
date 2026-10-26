//! PostgreSQL-backed jobs and transactional event delivery.
//!
//! Contexts write events to `platform.outbox` atomically with business state.
//! [`OutboxDispatcher::postgres`] makes committed events available to
//! [`EventConsumer::postgres`], which uses durable inbox deduplication, dead
//! letters and renewable PostgreSQL leases. [`Scheduler`] persists recurring
//! due times and retry state. Redis delivery remains an adapter; future brokers
//! implement [`EventPublisher`] without changing context event writers.

mod config;
mod consumer;
mod lease;
mod metrics;
mod outbox;
mod publisher;
mod recovery;
mod redis;
mod scheduler;
mod writer;

pub use config::RedisConfig;
pub use consumer::StreamConsumer as EventConsumer;
pub use consumer::{
    ConsumerHandler, HandlerError, StreamConsumer, TransactionalConsumerHandler, is_transient_error,
};
pub use metrics::{ConsumerMetrics, OutboxMetrics};
pub use outbox::{
    DeadLetterSummary, OutboxDispatcher, OutboxRecord, StreamName, list_dead_letters,
    replay_dead_letter,
};
pub use recovery::{ConsumerDeadLetter, list_consumer_dead_letters, replay_consumer_dead_letter};
pub use redis::{StreamsClient, connect_redis};
pub use scheduler::{FnJob, Job, Scheduler};
pub use writer::OutboxWriter;

pub use publisher::{EventPublisher, PostgresPublisher, PublishFuture};
