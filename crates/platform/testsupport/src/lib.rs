//! Test harness shared by every integration suite in the workspace.
//!
//! The suites used to start one Postgres container per test function and run
//! the full migration set into it — roughly 150 containers and 150 migration
//! runs for `cargo test --workspace`. This crate replaces that with:
//!
//! * **one** Postgres (and one Redis) per test process, or an externally
//!   provided server shared by every process (`scripts/test-db.sh`);
//! * a pre-migrated *template* database built once, from which each test
//!   clones its own database with `CREATE DATABASE … TEMPLATE …` — a file
//!   copy inside the server rather than 54 migrations over the wire.
//!
//! Isolation is unchanged: every call to [`test_db`] still hands back a
//! private, fully migrated database that no other test can observe. Redis
//! has no per-test equivalent of `CREATE DATABASE`, so its shared server is
//! divided by key name instead — see [`test_stream`].

mod postgres;
mod redis;

pub use postgres::{TestDb, blank_db, migrations_dir, test_db};
pub use redis::{redis_url, test_stream};

use std::{
    sync::Arc,
    sync::OnceLock,
    sync::atomic::{AtomicI64, Ordering},
};

use sanvi_kernel::{IdGenerationError, RawIdGenerator, SnowflakeId};

/// Deterministic Snowflake-compatible IDs for tests. Production processes
/// construct their generator from the configured Snowflake node.
#[derive(Debug)]
struct TestIdGenerator {
    next: AtomicI64,
}

impl TestIdGenerator {
    const fn new(first: i64) -> Self {
        Self {
            next: AtomicI64::new(first),
        }
    }
}

impl RawIdGenerator for TestIdGenerator {
    fn next_id(&self) -> Result<SnowflakeId, IdGenerationError> {
        let value = self.next.fetch_add(1, Ordering::Relaxed);
        SnowflakeId::try_from(value).map_err(|_| IdGenerationError::OutOfRange)
    }
}

/// One shared deterministic ID source per test *process* (not per call):
/// repeat calls within — or across — a test return the same underlying
/// counter, so a test that needs several distinct ids (e.g. two seeded
/// tenants) never collides on the same value.
pub fn test_ids() -> Arc<dyn RawIdGenerator> {
    static IDS: OnceLock<Arc<dyn RawIdGenerator>> = OnceLock::new();
    IDS.get_or_init(|| Arc::new(TestIdGenerator::new(1_000_000)))
        .clone()
}
