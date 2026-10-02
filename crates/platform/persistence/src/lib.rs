//! Sanvi persistence: Postgres connectivity, transactions and migrations.
//!
//! The application connects with a least-privilege role (`sanvi_app`) that
//! cannot create tables and cannot bypass row-level security. Migrations run
//! under a separate owner role via the CLI, never from the API process.

mod config;
mod health;
mod idempotency;
mod migrate;
mod pool;
mod pool_metrics;
mod tx;

pub use config::{DatabaseConfig, SslMode};
pub use health::check_database;
pub use idempotency::{IdempotencyHit, IdempotencyStore};
pub use migrate::Migrator;
pub use pool::build_pool;
pub use pool_metrics::{PoolMetrics, SAMPLE_INTERVAL};
pub use tx::{PlatformScope, ScopedTx, Tx, TxManager};

pub use sqlx::PgPool;
