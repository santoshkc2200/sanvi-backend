use chrono::{DateTime, Utc};

use crate::TenantId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    Email(String),
    TenantRoles {
        tenant_id: TenantId,
        roles: &'static [&'static str],
    },
    Platform,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentArg {
    Text(String),
    Int(i64),
    Timestamp(DateTime<Utc>),
}

impl From<String> for IntentArg {
    fn from(s: String) -> Self {
        IntentArg::Text(s)
    }
}

impl From<&str> for IntentArg {
    fn from(s: &str) -> Self {
        IntentArg::Text(s.to_string())
    }
}

impl From<i64> for IntentArg {
    fn from(i: i64) -> Self {
        IntentArg::Int(i)
    }
}

impl From<u64> for IntentArg {
    fn from(u: u64) -> Self {
        IntentArg::Int(u as i64)
    }
}

impl From<DateTime<Utc>> for IntentArg {
    fn from(dt: DateTime<Utc>) -> Self {
        IntentArg::Timestamp(dt)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationIntent {
    pub audience: Audience,
    pub purpose: &'static str,
    pub subject_key: String,
    pub body_key: String,
    pub args: Vec<(String, IntentArg)>,
}
