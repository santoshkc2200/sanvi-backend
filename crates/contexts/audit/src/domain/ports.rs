//! Ports (traits) implemented by driven adapters.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::PlatformScope;

use crate::domain::entry::{AuditEntry, AuditEntryData};
use crate::domain::error::AuditError;

/// Append an entry to the chain. Implementations chain *inside the caller's
/// transaction* (the writer takes the caller's `PlatformScope`), so the
/// audit record commits atomically with the state change it describes.
///
/// The writer takes the *data* (not a pre-sealed entry) because the
/// previous hash is only known at append time: it reads the current tail
/// under an advisory lock and seals the entry itself. Returns the
/// database-assigned sequence number.
#[async_trait]
pub trait AuditWriter: Send + Sync {
    async fn append(
        &self,
        scope: &mut PlatformScope<'_>,
        data: &AuditEntryData,
    ) -> Result<i64, AuditError>;
}

/// Pagination cursor into the chain (ordering: `seq`, the database-assigned
/// insertion order authority).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCursor {
    pub seq: i64,
}

/// Filters for audit queries. `None` means "no constraint".
#[derive(Debug, Clone, Default)]
pub struct AuditFilters {
    pub tenant_id: Option<TenantId>,
    pub actor_id: Option<UserId>,
    pub action: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

/// Read the chain. Default (newest first) serves the operator console;
/// `ascending` serves the verifier (chain order, genesis first).
#[async_trait]
pub trait AuditReader: Send + Sync {
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        filters: &AuditFilters,
        after: Option<AuditCursor>,
        limit: u32,
        ascending: bool,
    ) -> Result<(Vec<AuditEntry>, Option<AuditCursor>), AuditError>;
}
