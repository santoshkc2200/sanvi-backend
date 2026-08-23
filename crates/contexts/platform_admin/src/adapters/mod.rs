//! Adapters: infrastructure (driven) and transport (driving).

pub mod events;
pub mod gates;
pub mod http;
pub mod privacy_provider;
pub mod repo;
pub mod system;
pub mod verifier;

pub use events::OutboxEventPublisher;
pub use gates::{ApprovalGateAdapter, NoopApprovalGate, NoopPrivacyLock};
pub use repo::{
    SqlxFeeRepository, SqlxImpersonationGrantRepository, SqlxPendingApprovalRepository,
    SqlxTenantAdminViewRepository,
};
pub use system::{SystemClock, SystemIdGenerator};
pub use verifier::SqlxImpersonationVerifier;
