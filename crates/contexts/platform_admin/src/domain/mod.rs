//! Domain: entities, value objects, events, errors, ports.
//! No infrastructure imports allowed here (enforced by scripts/check-layering.sh).

pub mod approvals;
pub mod error;
pub mod events;
pub mod fees;
pub mod impersonation;
pub mod ports;
pub mod views;

pub use approvals::{ApprovalId, ApprovalStatus, PendingApproval};
pub use error::PlatformAdminError;
pub use events::{PlatformAdminEvent, PlatformAdminEventName};
pub use fees::ApplicationFeeConfig;
pub use impersonation::{ImpersonationGrant, ImpersonationGrantId, ImpersonationMode};
pub use ports::{
    ApplicationFeeAdmin, Clock, EntitlementAdmin, EventPublisher, GrantOverrideOutcome,
    IdGenerator, ImpersonationGrantRepository, OffboardingAdmin, OffboardingExportEvidence,
    OffboardingSnapshot, PendingApprovalRepository, PrivacyLock, TenantAdminViewRepository,
};
pub use views::{TenantAdminView, TenantViewCursor, TenantViewSearch};
