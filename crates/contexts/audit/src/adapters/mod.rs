//! Adapters: infrastructure (driven) and transport (driving).

pub mod archival_check;
pub mod http;
pub mod privacy_provider;
pub mod sqlx_audit;

pub use archival_check::SqlxArchivedRangeCheck;
pub use http::{AuditState, audit_routes};
pub use privacy_provider::{AuditPrivacyProvider, PrivacyAuditWriterAdapter};
pub use sqlx_audit::{SqlxAuditReader, SqlxAuditWriter};
