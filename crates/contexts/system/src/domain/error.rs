use thiserror::Error;

/// Errors this context's domain can raise.
///
/// Adapt the map onto these at the adapter boundary; infrastructure error
/// types (sqlx, redis, …) never cross into the domain.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SystemError {
    /// A readiness dependency is not answering.
    #[error("dependency {0} is unavailable: {1}")]
    DependencyUnavailable(String, String),

    /// The readiness probe could not finish in time.
    #[error("dependency {0} probe timed out")]
    DependencyTimeout(String),
}
