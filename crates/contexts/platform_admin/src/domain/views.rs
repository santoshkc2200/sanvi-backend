//! The operator's tenant read model: tenancy basics joined (in-process,
//! over ports — never cross-schema SQL) with identity and entitlement
//! summaries.

use chrono::{DateTime, Utc};
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One row of the platform admin tenant search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TenantAdminView {
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    pub slug: String,
    pub display_name: String,
    pub status: String,
    pub region: String,
    pub default_locale: String,
    pub created_at: DateTime<Utc>,
    /// Active members (identity projection).
    pub member_count: u32,
    /// Non-expired entitlement overrides (access context).
    pub override_count: u32,
    /// Active impersonation grants into this tenant.
    pub active_impersonations: u32,
}

/// The search query shape for the operator console.
#[derive(Debug, Clone, Default)]
pub struct TenantViewSearch {
    /// Case-insensitive substring match on slug/display name.
    pub query: Option<String>,
    pub status: Option<String>,
    pub after: Option<TenantViewCursor>,
    pub limit: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantViewCursor {
    pub created_at: DateTime<Utc>,
    pub tenant_id: TenantId,
}
