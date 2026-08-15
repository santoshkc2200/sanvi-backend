use thiserror::Error;

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
