//! Adapters: infrastructure (driven) and transport (driving).

pub mod http;
pub mod privacy_provider;
pub mod sqlx_audit;

pub use http::{AuditState, audit_routes};
pub use privacy_provider::{AuditPrivacyProvider, PrivacyAuditWriterAdapter};
pub use sqlx_audit::{SqlxAuditReader, SqlxAuditWriter};
