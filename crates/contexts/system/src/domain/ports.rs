use std::time::Duration;

use crate::domain::error::SystemError;

/// Port for a dependency probe, implemented by driven adapters
/// (Postgres, Redis, …) and by fakes in tests.
///
/// Async-trait-free by design: the port is a plain async fn in a trait
/// implemented via the context's own adapters, keeping the domain free of
/// proc-macro dependencies.
#[async_trait::async_trait]
pub trait HealthProbe: Send + Sync {
    /// Stable name reported in readiness output.
    fn name(&self) -> &'static str;

    /// Check the dependency. Implementations must respect `budget`.
    async fn check(&self, budget: Duration) -> Result<(), SystemError>;
}
