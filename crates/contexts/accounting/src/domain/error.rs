use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AccountingError {
    #[error("invalid accounting input: {0}")]
    Invalid(&'static str),
    #[error("accounting book or connection not found")]
    NotFound,
    #[error("accounting access denied")]
    Forbidden,
    #[error("accounting state changed; reload before retrying")]
    StaleRevision,
    #[error("accounting operation conflicts with current state: {0}")]
    Conflict(&'static str),
    #[error("provider verification is unavailable")]
    VerificationUnavailable,
    #[error("historical imports require a separate reviewed migration")]
    HistoricalImport,
    #[error("source event was already assigned with different content")]
    IdempotencyConflict,
    #[error("accounting storage failure: {0}")]
    Storage(String),
}
