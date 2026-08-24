//! Provider-neutral connection health and refresh transitions.
//!
//! Scheduler and HTTP adapters consume these transitions. Keeping permanent
//! token failure distinct from a retryable outage prevents a revoked token from
//! becoming an endless five-minute retry loop.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Active,
    Disconnected,
    Expired,
}

impl ConnectionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disconnected => "disconnected",
            Self::Expired => "expired",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "disconnected" => Some(Self::Disconnected),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionHealth {
    pub can_sync: bool,
    pub can_upload_conversions: bool,
    pub token_expires_at: Option<DateTime<Utc>>,
    pub scopes_missing: Vec<String>,
    pub last_error: Option<String>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub reconnect_required: bool,
}

impl ConnectionHealth {
    pub fn from_connection(
        status: ConnectionStatus,
        token_expires_at: Option<DateTime<Utc>>,
        granted_scopes: &[String],
        required_scopes: &[String],
        last_error: Option<String>,
        last_synced_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Self {
        let mut scopes_missing = required_scopes
            .iter()
            .filter(|scope| !granted_scopes.contains(scope))
            .cloned()
            .collect::<Vec<_>>();
        scopes_missing.sort();
        let token_valid = token_expires_at.is_none_or(|expires_at| expires_at > now);
        let active = status == ConnectionStatus::Active && token_valid;
        Self {
            can_sync: active && scopes_missing.is_empty(),
            can_upload_conversions: active && scopes_missing.is_empty(),
            token_expires_at,
            scopes_missing,
            last_error,
            last_synced_at,
            reconnect_required: !active,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailureKind {
    Retryable,
    Revoked,
    PermissionRevoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshTransition {
    pub status: ConnectionStatus,
    pub schedule_retry: bool,
    pub pause_dependent_jobs: bool,
    pub notify_tenant: bool,
}

/// Translate one refresh result into an idempotent scheduling decision.
/// `already_notified` is persisted by the repository so repeated worker runs
/// cannot turn a revoked credential into notification spam.
pub fn refresh_failure_transition(
    failure: RefreshFailureKind,
    already_notified: bool,
) -> RefreshTransition {
    match failure {
        RefreshFailureKind::Retryable => RefreshTransition {
            status: ConnectionStatus::Active,
            schedule_retry: true,
            pause_dependent_jobs: false,
            notify_tenant: false,
        },
        RefreshFailureKind::Revoked | RefreshFailureKind::PermissionRevoked => RefreshTransition {
            status: ConnectionStatus::Disconnected,
            schedule_retry: false,
            pause_dependent_jobs: true,
            notify_tenant: !already_notified,
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    #[test]
    fn missing_granted_scope_requires_reconsent_and_names_the_scope() {
        let now = Utc::now();
        let health = ConnectionHealth::from_connection(
            ConnectionStatus::Active,
            Some(now + Duration::hours(1)),
            &["ads.read".to_string()],
            &["ads.read".to_string(), "ads.manage".to_string()],
            None,
            None,
            now,
        );
        assert!(!health.can_sync);
        assert_eq!(health.scopes_missing, ["ads.manage"]);
        assert!(!health.reconnect_required);
    }

    #[test]
    fn revoked_refresh_pauses_without_rescheduling_or_repeat_notification() {
        let first = refresh_failure_transition(RefreshFailureKind::Revoked, false);
        assert_eq!(first.status, ConnectionStatus::Disconnected);
        assert!(first.pause_dependent_jobs);
        assert!(first.notify_tenant);
        assert!(!first.schedule_retry);

        let repeated = refresh_failure_transition(RefreshFailureKind::Revoked, true);
        assert!(!repeated.schedule_retry);
        assert!(!repeated.notify_tenant);
    }
}
