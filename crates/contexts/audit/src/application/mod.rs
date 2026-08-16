//! Application: use cases for the audit context.

pub mod list_audit;
pub mod verify_chain;

pub use list_audit::{AuditPage, ListAuditLog};
pub use verify_chain::{ChainVerificationReport, VerifyAuditChain};

/// Maps infrastructure failures into the domain vocabulary at the boundary.
pub(crate) fn storage(err: impl std::fmt::Display) -> crate::domain::AuditError {
    crate::domain::AuditError::Storage(err.to_string())
}
