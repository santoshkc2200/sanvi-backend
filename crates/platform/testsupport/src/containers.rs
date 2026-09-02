//! Shared-container startup, used by both the Postgres and Redis harnesses.
//!
//! `cargo nextest` runs every test in its own process. Left to itself each
//! process starts its own anonymous container, so one workspace run stood up
//! dozens of servers at once and starved the Docker daemon. Naming a container
//! and asking for reuse collapses that to one server per image, but reuse
//! alone is not enough:
//!
//! * Reuse de-duplicates by name, it does not serialize creation. Processes
//!   that look for the container at the same moment all miss, all try to
//!   create it, and every loser gets `409 Conflict`.
//! * Only the process that *creates* a container runs testcontainers' wait
//!   condition. A process that attaches to one skips that wait entirely, so it
//!   has to establish readiness itself.
//!
//! [`start_shared`] handles the first; each caller supplies its own readiness
//! probe for the second, because "ready" is protocol-specific.

use std::time::Duration;

use testcontainers::core::error::TestcontainersError;
use testcontainers::{ContainerAsync, ContainerRequest, Image, runners::AsyncRunner};

const CONFLICT_RETRIES: u32 = 40;
const CONFLICT_BACKOFF: Duration = Duration::from_millis(250);

/// Start the named container, or attach to the one another process started.
///
/// `request` is called per attempt rather than taken once, because starting
/// consumes the request and a retry needs an identical one.
pub(crate) async fn start_shared<I, F>(image: &str, request: F) -> ContainerAsync<I>
where
    I: Image,
    F: Fn() -> ContainerRequest<I>,
{
    for attempt in 0..=CONFLICT_RETRIES {
        match request().start().await {
            Ok(node) => return node,
            // Losing the creation race is not a failure: the container the
            // winner made is exactly the one this process wanted, and the
            // retry's reuse lookup finds it.
            Err(error) if attempt < CONFLICT_RETRIES && is_name_conflict(&error) => {
                tokio::time::sleep(CONFLICT_BACKOFF).await;
            }
            Err(error) => panic!("failed to start {image}: {error}"),
        }
    }
    unreachable!("the loop either returns a container or panics")
}

/// A 409 from Docker means another process created the container between our
/// reuse lookup and our create. Matched on the message because the status
/// survives only inside the rendered server response.
fn is_name_conflict(error: &TestcontainersError) -> bool {
    let rendered = error.to_string();
    rendered.contains("already in use") || rendered.contains("Conflict")
}
