//! Sanvi messaging: Redis Streams client and the transactional outbox
//! dispatcher.
//!
//! Contexts write domain events to `platform.outbox` in the same transaction
//! as their state change. [`OutboxDispatcher`] (running in `apps/worker`)
//! relays unpublished events to Redis Streams; consumers read from streams as
//! consumer groups and deduplicate on the event id.

mod config;
mod outbox;
mod redis;

pub use config::RedisConfig;
pub use outbox::{OutboxDispatcher, OutboxRecord, StreamName};
pub use redis::{StreamsClient, connect_redis};
