//! Tenant accounting destinations and controlled provider cutovers.
//!
//! This context owns integration state, never a merchant's general ledger or stock.
//! Provider authorization and HTTP/UI composition are deliberately not enabled yet.
pub mod adapters;
pub mod application;
pub mod domain;
