//! Domain: entities, value objects, errors, ports.
//! No infrastructure imports allowed here (enforced by scripts/check-layering.sh).

pub mod chain;
pub mod entry;
pub mod error;
pub mod ports;

pub use chain::{verify_chain, verify_segment};
pub use entry::{ActorType, AuditEntry, AuditEntryData, GENESIS_HASH};
pub use error::AuditError;
pub use ports::{
    ArchivedRangeCheck, ArchivedRangeWindow, AuditCursor, AuditFilters, AuditReader, AuditWriter,
};
