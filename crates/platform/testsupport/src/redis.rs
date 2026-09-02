use std::sync::atomic::{AtomicU64, Ordering};

use testcontainers::{
    ContainerAsync, ContainerRequest, GenericImage, ImageExt, ReuseDirective,
    core::{IntoContainerPort, WaitFor},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;

/// Name every test process converges on, so a bare `cargo nextest run` ends up
/// with one Redis rather than one per process. Same reasoning as the Postgres
/// harness — see [`crate::containers`].
const SHARED_CONTAINER_NAME: &str = "sanvi_test_redis";

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
            let node = crate::containers::start_shared("redis", shared_redis_request).await;
            let port = await_ready_redis(&node).await;
            // Same lifetime rule as Postgres: outlives every test in this
            // process, reaped by `scripts/test-db.sh prune`. With reuse this
            // leaves one named container per machine, not one per process.
            std::mem::forget(node);
            port
        })
        .await
}

fn shared_redis_request() -> ContainerRequest<GenericImage> {
    // Redis 8, matching production (docker-compose.yml). The
    // `testcontainers_modules` image is pinned to 5.0, which predates
    // the `XAUTOCLAIM` the consumer's crash recovery relies on.
    GenericImage::new("redis", "8-alpine")
        .with_exposed_port(6379.tcp())
        // Only the process that creates the container observes this; every
        // process that attaches to it waits via `await_ready_redis` instead.
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_container_name(SHARED_CONTAINER_NAME)
        .with_reuse(ReuseDirective::Always)
}

/// Wait until the shared server answers `PING`, and return its port.
///
/// A process that attached to a container someone else created never ran the
/// wait condition above, so it proves readiness itself. `PING` is the whole
/// check: a server that replies `+PONG` has published its port, finished
/// starting, and is accepting commands. Raw TCP because this crate has no
/// Redis client — and does not need one for two frames.
async fn await_ready_redis(node: &ContainerAsync<GenericImage>) -> u16 {
    const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    const READY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    let mut last_error = String::new();
    while std::time::Instant::now() < deadline {
        match node.get_host_port_ipv4(6379).await {
            Ok(port) => match ping(port).await {
                Ok(()) => return port,
                Err(error) => last_error = error,
            },
            Err(error) => last_error = error.to_string(),
        }
        tokio::time::sleep(READY_BACKOFF).await;
    }
    panic!("shared redis never became ready within {READY_TIMEOUT:?}: {last_error}");
}

async fn ping(port: u16) -> Result<(), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"PING\r\n")
        .await
        .map_err(|error| error.to_string())?;
    let mut reply = [0u8; 7];
    let read = stream
        .read(&mut reply)
        .await
        .map_err(|error| error.to_string())?;
    if reply[..read].starts_with(b"+PONG") {
        Ok(())
    } else {
        Err(format!(
            "unexpected reply to PING: {:?}",
            String::from_utf8_lossy(&reply[..read])
        ))
    }
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
