//! Application: use cases for the tenancy context.
//!
//! Every use case owns its transactions: platform-scoped work goes through
//! `PlatformScope::begin_unscoped` (audited), tenant-scoped work through
//! `ScopedTx::begin_for` (RLS-filtered). Domain events are written to the
//! outbox in the same transaction as the state change.

pub mod check_slug_availability;
pub mod get_tenant;
pub mod list_tenants;
pub mod offboarding;
pub mod provision_tenant;
pub mod purge_sweep;
pub mod resolve_tenant;
pub mod tenant_lifecycle;
pub mod tenant_settings;

pub use check_slug_availability::{CheckSlugAvailability, SlugAvailability};
pub use get_tenant::{GetTenant, TenantView};
pub use list_tenants::{ListTenants, TenantListView};
pub use offboarding::{
    AdvanceOffboarding, GetOffboarding, OffboardingView, RequestOffboarding, SystemClock,
};
pub use provision_tenant::{ProvisionTenant, ProvisionTenantCommand};
pub use purge_sweep::PurgeSweep;
pub use resolve_tenant::ResolveTenant;
pub use tenant_lifecycle::{
    ActivateTenant, ArchiveTenant, LifecycleAction, ResumeTenant, SuspendTenant,
    SuspendTenantRequest,
};
pub use tenant_settings::{
    GetTenantSettings, SettingsUpdate, TenantSettingsOutput, UpdateTenantSettings,
};

/// Maps infrastructure failures into the domain vocabulary at the boundary.
pub(crate) fn storage(err: impl std::fmt::Display) -> crate::domain::TenancyError {
    crate::domain::TenancyError::Storage(err.to_string())
}
