//! The offboarding aggregate (TASK-028, FR-1131): the durable record of
//! one tenant's export-then-purge.
//!
//! The API creates the request (idempotent by key); the CLI executor
//! advances it. The state machine is deliberately one-way past the
//! export: `pending → exporting → exported → purging → purged`, with
//! `failed` as the terminal error state — the purge is not reversible,
//! so there is no path back.

use chrono::{DateTime, Utc};

use crate::domain::tenant::TenantId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffboardingStatus {
    Pending,
    Exporting,
    Exported,
    Purging,
    Purged,
    Failed,
}

impl OffboardingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Exporting => "exporting",
            Self::Exported => "exported",
            Self::Purging => "purging",
            Self::Purged => "purged",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "exporting" => Some(Self::Exporting),
            "exported" => Some(Self::Exported),
            "purging" => Some(Self::Purging),
            "purged" => Some(Self::Purged),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// The verified-export evidence. The purge step is unreachable until
/// both fields are present — the hard precondition replacing the
/// two-person confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedExport {
    pub path: String,
    pub checksum: String,
}

#[derive(Debug, Clone)]
pub struct OffboardingRequest {
    pub id: i64,
    pub tenant_id: TenantId,
    pub idempotency_key: String,
    pub status: OffboardingStatus,
    pub requested_by: Option<i64>,
    pub export: Option<VerifiedExport>,
    pub purge_report: Option<serde_json::Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl OffboardingRequest {
    /// The one legal transition table. A request that is already
    /// terminal answers `None` — replaying the same completion is a
    /// no-op, not an error (offboarding is idempotent).
    pub fn can_advance_to(&self, next: OffboardingStatus) -> bool {
        use OffboardingStatus::*;
        matches!(
            (self.status, next),
            (Pending, Exporting)
                | (Exporting, Exported)
                | (Exporting, Failed)
                | (Exported, Purging)
                | (Purging, Purged)
                | (Purging, Failed)
                | (Failed, Failed)
                | (Purged, Purged)
        )
    }

    /// The purge gate (FR-1131): `purging` is unreachable without a
    /// verified export. Replacing the two-person confirmation: dry-run,
    /// verified export, typed confirmation.
    pub fn export_verified(&self) -> bool {
        self.export.is_some()
    }
}
