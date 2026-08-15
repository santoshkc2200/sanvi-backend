use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Whether a dependency is healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    Ok,
    Degraded,
}

/// Result of probing one dependency for readiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DependencyStatus {
    /// Stable dependency name, e.g. `database`, `redis`.
    #[schema(example = "database")]
    pub name: String,
    pub state: HealthState,
    /// Human-readable detail, present when degraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl DependencyStatus {
    pub fn healthy(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: HealthState::Ok,
            detail: None,
        }
    }

    pub fn degraded(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: HealthState::Degraded,
            detail: Some(detail.into()),
        }
    }
}

/// Aggregate readiness answer: every dependency's status plus the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Readiness {
    /// `ok` when every dependency is healthy.
    pub status: HealthState,
    pub checks: Vec<DependencyStatus>,
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        self.status == HealthState::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_verdict_follows_checks() {
        let ready = Readiness {
            status: HealthState::Ok,
            checks: vec![DependencyStatus::healthy("database")],
        };
        assert!(ready.is_ready());

        let degraded = Readiness {
            status: HealthState::Degraded,
            checks: vec![DependencyStatus::degraded("redis", "connection refused")],
        };
        assert!(!degraded.is_ready());
    }

    #[test]
    fn dependency_status_serde_omits_detail_when_healthy() {
        let status = DependencyStatus::healthy("database");
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#"{"name":"database","state":"ok"}"#);
    }
}
