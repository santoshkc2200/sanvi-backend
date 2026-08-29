//! Real system adapters: wall clock and UUIDv7 id generation.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::RawIdGenerator;

use crate::domain::approvals::ApprovalId;
use crate::domain::impersonation::ImpersonationGrantId;
use crate::domain::ports::{Clock, IdGenerator};

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Clone)]
pub struct SystemIdGenerator {
    ids: Arc<dyn RawIdGenerator>,
}

impl SystemIdGenerator {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }
}

impl IdGenerator for SystemIdGenerator {
    fn next_grant_id(&self) -> Result<ImpersonationGrantId, crate::domain::PlatformAdminError> {
        self.ids
            .next_id()
            .map(ImpersonationGrantId::from_snowflake)
            .map_err(Into::into)
    }

    fn next_approval_id(&self) -> Result<ApprovalId, crate::domain::PlatformAdminError> {
        self.ids
            .next_id()
            .map(ApprovalId::from_snowflake)
            .map_err(Into::into)
    }
}
