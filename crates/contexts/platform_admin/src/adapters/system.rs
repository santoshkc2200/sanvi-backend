//! Real system adapters: wall clock and UUIDv7 id generation.

use chrono::{DateTime, Utc};

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

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemIdGenerator;

impl IdGenerator for SystemIdGenerator {
    fn next_grant_id(&self) -> ImpersonationGrantId {
        ImpersonationGrantId::new()
    }

    fn next_approval_id(&self) -> ApprovalId {
        ApprovalId::new()
    }
}
