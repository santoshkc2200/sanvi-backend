//! Filtered audit queries for the platform admin console.

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::application::storage;
use crate::domain::{
    ArchivedRangeCheck, AuditCursor, AuditEntry, AuditError, AuditFilters, AuditReader,
};

pub const DEFAULT_PAGE_LIMIT: u32 = 50;
pub const MAX_PAGE_LIMIT: u32 = 200;

/// One page of audit entries plus the cursor for the next page.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    pub next_cursor: Option<AuditCursor>,
}

pub struct ListAuditLog {
    tx_manager: TxManager,
    reader: Arc<dyn AuditReader>,
    /// TASK-028 (FR-1129): when the archival feature is on, a read over
    /// an archived window answers archived-range instead of returning
    /// the surviving rows as a silent partial.
    archive_check: Option<Arc<dyn ArchivedRangeCheck>>,
}

impl ListAuditLog {
    pub fn new(tx_manager: TxManager, reader: Arc<dyn AuditReader>) -> Self {
        Self {
            tx_manager,
            reader,
            archive_check: None,
        }
    }

    /// Enable the archived-range refusal (composition root wires this
    /// only when the archival feature is on).
    pub fn with_archive_check(mut self, check: Arc<dyn ArchivedRangeCheck>) -> Self {
        self.archive_check = Some(check);
        self
    }

    #[tracing::instrument(name = "audit.list_audit_log", skip_all)]
    pub async fn execute(
        &self,
        filters: AuditFilters,
        after: Option<AuditCursor>,
        limit: Option<u32>,
    ) -> Result<AuditPage, AuditError> {
        let limit =
            sanvi_kernel::pagination::clamp_page_size(limit, DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT);
        let mut scope = self
            .tx_manager
            .begin_unscoped("audit_list", "platform-admin")
            .await
            .map_err(storage)?;
        if let Some(check) = &self.archive_check {
            let overlapping = check
                .overlapping(&mut scope, filters.tenant_id, filters.since, filters.until)
                .await?;
            if !overlapping.is_empty() {
                // Rolling the read transaction back: the refusal must not
                // leave anything open behind it.
                scope.rollback().await.map_err(storage)?;
                return Err(AuditError::ArchivedRange(filters.since, filters.until));
            }
        }
        let (entries, next_cursor) = self
            .reader
            .list(&mut scope, &filters, after, limit, false)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(AuditPage {
            entries,
            next_cursor,
        })
    }
}
