//! Adapters: infrastructure (driven) and transport (driving).

pub mod http;
pub mod sqlx_audit;

pub use http::{AuditState, audit_routes};
pub use sqlx_audit::{SqlxAuditReader, SqlxAuditWriter};
