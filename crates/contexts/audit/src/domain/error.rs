use sanvi_kernel::SnowflakeId;
use thiserror::Error;

/// Domain errors for the `audit` context.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuditError {
    #[error("invalid actor type: {0}")]
    InvalidActorType(String),

    #[error("invalid action: {0}")]
    InvalidAction(String),

    #[error("audit chain mismatch at entry {entry_id} (position {position}): {detail}")]
    ChainMismatch {
        entry_id: SnowflakeId,
        position: usize,
        detail: String,
    },

    #[error("invalid audit cursor: {0}")]
    InvalidCursor(String),

    #[error("storage failure: {0}")]
    Storage(String),

    /// TASK-028 (FR-1129): the requested window overlaps archived cold
    /// storage. The rows exist — in the archive, restorable through the
    /// documented path — which is exactly why a partial answer would lie.
    #[error(
        "the requested range {0:?}..{1:?} overlaps archived storage; restore it first (docs/runbooks/archival.md)"
    )]
    ArchivedRange(
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ),
}
