use std::sync::atomic::{AtomicU64, Ordering};

use testcontainers::{
    GenericImage,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio::sync::OnceCell;

static REDIS_PORT: OnceCell<u16> = OnceCell::const_new();
static KEY_SEQ: AtomicU64 = AtomicU64::new(0);

async fn redis_port() -> u16 {
    *REDIS_PORT
        .get_or_init(|| async {
            if let Ok(port) = std::env::var("SANVI_TEST_REDIS_PORT") {
                return port
                    .parse()
                    .expect("SANVI_TEST_REDIS_PORT must be a port number");
            }
            // Redis 8, matching production (docker-compose.yml). The
            // `testcontainers_modules` image is pinned to 5.0, which predates
            // the `XAUTOCLAIM` the consumer's crash recovery relies on.
            let node = GenericImage::new("redis", "8-alpine")
                .with_exposed_port(6379.tcp())
                .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
                .start()
                .await
                .expect("failed to start redis");
            let port = node.get_host_port_ipv4(6379).await.expect("redis port");
            // Same lifetime rule as Postgres: outlives every test in this
            // process, reaped by `scripts/test-db.sh prune`.
            std::mem::forget(node);
            port
        })
        .await
}

/// URL of the Redis every test shares.
///
/// Unlike Postgres, which gets a whole database per test, Redis is one key
/// space: a stock server has sixteen logical databases and the suite has far
/// more tests than that, so tests are kept apart by key name instead — see
/// [`test_stream`]. Every key a test touches must come from there.
pub async fn redis_url() -> String {
    format!("redis://127.0.0.1:{}", redis_port().await)
}

/// A stream name no other test uses.
///
/// Unique within a process (the counter) and across every process and run
/// that ever shares this server (the UUID). Both halves are needed:
/// `cargo nextest` runs each test in its own process, so a per-process
/// counter alone hands every test the same name, and the server outlives the
/// run, so the names must not repeat across runs either.
///
/// `name` is a readable suffix, so a leftover key says which test left it.
pub fn test_stream(name: &str) -> String {
    format!(
        "sanvi.t.{}_{}.{name}",
        uuid::Uuid::now_v7().simple(),
        KEY_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}
