//! The archived-range check over `platform.archives` (TASK-028, FR-1129).
//!
//! Thin bridge from the audit domain's port to the archival crate's
//! registry queries: this context stays free of storage details, the
//! archival crate stays free of audit semantics.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::PlatformScope;

use crate::domain::{ArchivedRangeCheck, ArchivedRangeWindow, AuditError};

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxArchivedRangeCheck;

impl SqlxArchivedRangeCheck {
    pub const fn new() -> Self {
        Self
    }
}

fn storage(err: impl std::fmt::Display) -> AuditError {
    AuditError::Storage(err.to_string())
}

#[async_trait]
impl ArchivedRangeCheck for SqlxArchivedRangeCheck {
    async fn overlapping(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Option<TenantId>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<ArchivedRangeWindow>, AuditError> {
        let conn = &mut **scope.tx_mut().as_inner();
        let records = sanvi_archival::ranges::overlapping_reads(
            conn,
            "audit",
            "entries",
            tenant_id.map(|id| id.as_i64()),
            since,
            until,
        )
        .await
        .map_err(storage)?;
        Ok(records
            .into_iter()
            .map(|record| ArchivedRangeWindow {
                range_start: record.range_start,
                range_end: record.range_end,
            })
            .collect())
    }
}
