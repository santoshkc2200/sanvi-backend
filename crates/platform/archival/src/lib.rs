//! TASK-028 (FR-1129): archival — the mover, the cold-storage adapter and
//! the archived-range read path.
//!
//! Archival without a rehearsed restore is deletion with extra steps, so
//! the two are one module: [`mover::ArchiveMover`] moves a range of a
//! table to encrypted cold storage and records it in `platform.archives`;
//! [`mover::ArchiveMover::restore`] brings a range back and proves the
//! rows (ordering and checksum included) are identical. A read over an
//! archived range is never a silent partial — [`ranges`] answers whether
//! a range is archived, and the audit read path surfaces that as a typed
//! archived-range response instead of quietly returning the surviving
//! rows.
//!
//! Rows travel as one canonical JSON document per archive: the full row
//! set ordered by the range key, serialized through Postgres's own
//! `to_jsonb`/`jsonb_populate_recordset` round trip (the same mechanism
//! `tenant-restore` proves byte-exact restores with). The checksum is
//! sha256 over that document, so "identical rows" is a comparable fact,
//! not a promise.
//!
//! Archives are per `(table, range, tenant)`: platform tables with a
//! tenant key archive one tenant's rows at a time, so a departing
//! tenant's purge can destroy exactly its archives and a neighbour's
//! stay byte-identical. Rows with no tenant (platform scope) archive
//! under a NULL tenant scope.
//!
//! Encryption is the phase-05 envelope scheme's passphrase form
//! (PBKDF2-HMAC-SHA256 + AES-256-GCM, `privacy::EnvelopeCrypto::
//! encrypt_with_passphrase`); this crate defines only the
//! [`storage::ArchiveCipher`] port, and the composition roots bridge it
//! to the privacy implementation so platform code never depends on a
//! bounded context.
//!
//! The cold object store is a port too ([`storage::ColdStorage`]): the
//! mover writes to a local encrypted volume behind the same interface,
//! so the storage adapter is a swap when there is somewhere to swap to.

pub mod config;
pub mod mover;
pub mod ranges;
pub mod storage;
pub mod sweep;

/// Errors of the archival module. `RangeArchived` and `LoudFailure` are
/// the contract's two halves: overlap is refused loudly before any
/// destructive step, and any post-purge inconsistency fails the job
/// rather than completing quietly.
#[derive(Debug, thiserror::Error)]
pub enum ArchivalError {
    #[error("storage failure: {0}")]
    Storage(String),

    #[error("cold storage failure: {0}")]
    Cold(String),

    #[error("encryption failure: {0}")]
    Cipher(String),

    /// A destructive step refused: the range is already archived, or the
    /// archive is not in the state the step requires.
    #[error(
        "range {range_start}..{range_end} of {schema_name}.{table_name} is already archived (archive id {archive_id}); restore it first"
    )]
    RangeArchived {
        range_start: chrono::DateTime<chrono::Utc>,
        range_end: chrono::DateTime<chrono::Utc>,
        schema_name: String,
        table_name: String,
        archive_id: i64,
    },

    #[error("archive {0} not found")]
    NotFound(i64),

    #[error("archive {archive_id} is {status}, not {expected}")]
    WrongStatus {
        archive_id: i64,
        status: String,
        expected: String,
    },

    /// The restored rows do not checksum to what was archived. This is
    /// never retried blindly — the archive and the table disagree about
    /// history.
    #[error(
        "restore checksum mismatch for archive {archive_id}: archived {archived}, restored {restored}"
    )]
    ChecksumMismatch {
        archive_id: i64,
        archived: String,
        restored: String,
    },

    #[error("archive {archive_id} restored {restored} rows but archived {archived}")]
    RowCountMismatch {
        archive_id: i64,
        archived: i64,
        restored: i64,
    },
}

impl ArchivalError {
    /// Accepts any error by display, so `map_err(ArchivalError::storage)`
    /// works uniformly across sqlx/anyhow/io errors.
    pub fn storage(detail: impl std::fmt::Display) -> Self {
        Self::Storage(detail.to_string())
    }
}
