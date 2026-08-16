//! System adapters for the access context: id generation and the wall
//! clock.

use chrono::{DateTime, Utc};

use crate::domain::ports::{Clock, IdGenerator};
use crate::domain::roles::RoleId;

/// Real role-id generation (UUIDv7 via the shared kernel).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRoleIdGenerator;

impl IdGenerator for SystemRoleIdGenerator {
    fn next_role_id(&self) -> RoleId {
        RoleId::new()
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
