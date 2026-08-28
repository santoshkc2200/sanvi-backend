//! System adapters for the access context: id generation and the wall
//! clock.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::RawIdGenerator;

use crate::domain::ports::{Clock, IdGenerator};
use sanvi_kernel::RoleId;

/// Real role-id generation (UUIDv7 via the shared kernel).
#[derive(Clone)]
pub struct SystemRoleIdGenerator { ids: Arc<dyn RawIdGenerator> }

impl SystemRoleIdGenerator {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self { Self { ids } }
}

impl IdGenerator for SystemRoleIdGenerator {
    fn next_role_id(&self) -> Result<RoleId, crate::domain::AccessError> {
        self.ids.next_id().map(RoleId::from_snowflake).map_err(Into::into)
    }
}

/// Real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
