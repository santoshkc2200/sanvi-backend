//! Filtered audit queries for the platform admin console.

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::application::storage;
use crate::domain::{AuditCursor, AuditEntry, AuditError, AuditFilters, AuditReader};

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
}

impl ListAuditLog {
    pub fn new(tx_manager: TxManager, reader: Arc<dyn AuditReader>) -> Self {
        Self { tx_manager, reader }
    }

    #[tracing::instrument(name = "audit.list_audit_log", skip_all)]
    pub async fn execute(
        &self,
        filters: AuditFilters,
        after: Option<AuditCursor>,
        limit: u32,
    ) -> Result<AuditPage, AuditError> {
        let limit = limit.clamp(1, MAX_PAGE_LIMIT);
        let mut scope = self
            .tx_manager
            .begin_unscoped("audit_list", "platform-admin")
            .await
            .map_err(storage)?;
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
