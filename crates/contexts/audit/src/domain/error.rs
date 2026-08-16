use thiserror::Error;
use uuid::Uuid;

/// Domain errors for the `audit` context.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuditError {
    #[error("invalid actor type: {0}")]
    InvalidActorType(String),

    #[error("invalid action: {0}")]
    InvalidAction(String),

    #[error("audit chain mismatch at entry {entry_id} (position {position}): {detail}")]
    ChainMismatch {
        entry_id: Uuid,
        position: usize,
        detail: String,
    },

    #[error("invalid audit cursor: {0}")]
    InvalidCursor(String),

    #[error("storage failure: {0}")]
    Storage(String),
}
