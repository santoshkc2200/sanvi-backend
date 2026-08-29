use thiserror::Error;

/// Failures that prevent production Snowflake allocation.
#[derive(Debug, Error)]
pub enum IdGenerationError {
    #[error("snowflake node id must be between 1 and 1023, got {0}")]
    InvalidNode(u16),
    #[error("system clock moved backwards from {last_ms}ms to {now_ms}ms since epoch")]
    ClockSkew { last_ms: u64, now_ms: u64 },
    #[error("snowflake value exceeded signed 64-bit storage")]
    OutOfRange,
}

/// Errors the shared kernel can produce.
///
/// Contexts map these into their own domain errors at the boundary; this
/// type never carries infrastructure errors.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum KernelError {
    #[error("invalid identifier: {value}")]
    InvalidId { value: String },

    #[error("invalid pagination: {reason}")]
    InvalidPagination { reason: String },
}
