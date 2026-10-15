//! Three-way campaign reconciliation.
//!
//! A remote campaign is never compared directly with our intent.  Doing so
//! cannot distinguish a platform edit from a local edit awaiting sync.  The
//! last remote snapshot is the required third value.

use serde::{Deserialize, Serialize};

use super::{Campaign, DriftState};

/// Persisted state needed to reconcile one campaign without overwriting a
/// native platform edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignReconciliationState {
    pub intent: Campaign,
    pub last_known_remote: Campaign,
}

impl CampaignReconciliationState {
    pub fn new(campaign: Campaign) -> Self {
        Self {
            intent: campaign.clone(),
            last_known_remote: campaign,
        }
    }

    /// Reconcile a newly fetched remote campaign.
    ///
    /// A remote change becomes drift whenever it contradicts current Sanvi
    /// intent. A matching remote change only advances the snapshot. Existing
    /// drift is recomputed from intent and remote state, so an unrelated
    /// platform change cannot clear it.
    pub fn reconcile(&mut self, remote: Campaign) -> ReconciliationResult {
        let remote_changes = changed_fields(&self.last_known_remote, &remote);
        if remote_changes.is_empty() {
            // A successful reapply can match the existing snapshot exactly.
            // Recompute the marker even when no remote fields changed.
            let conflicts = changed_fields(&self.intent, &remote);
            self.intent.drift = DriftState {
                drifted: !conflicts.is_empty(),
                changed_fields: conflicts,
            };
            return ReconciliationResult::Unchanged;
        }

        self.last_known_remote = remote;
        let conflicts = changed_fields(&self.intent, &self.last_known_remote);
        if conflicts.is_empty() {
            self.intent.drift = DriftState {
                drifted: false,
                changed_fields: Vec::new(),
            };
            ReconciliationResult::RemoteUpdated { remote_changes }
        } else {
            self.intent.drift = DriftState {
                drifted: true,
                changed_fields: conflicts.clone(),
            };
            ReconciliationResult::Drifted { fields: conflicts }
        }
    }

    /// Explicitly accept the platform's state as new Sanvi intent.
    pub fn keep_theirs(&mut self) {
        self.intent = self.last_known_remote.clone();
        self.intent.drift = DriftState {
            drifted: false,
            changed_fields: Vec::new(),
        };
    }

    /// Mark a successfully pushed local intent as the remote snapshot.  The
    /// caller must invoke this only after the provider mutation succeeds.
    pub fn record_reapplied_ours(&mut self) {
        self.intent.drift = DriftState {
            drifted: false,
            changed_fields: Vec::new(),
        };
        self.last_known_remote = self.intent.clone();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationResult {
    Unchanged,
    RemoteUpdated { remote_changes: Vec<String> },
    Drifted { fields: Vec<String> },
}

const RECONCILED_FIELDS: [&str; 6] = [
    "name",
    "objective",
    "status",
    "budget",
    "schedule",
    "ad_groups",
];

fn changed_fields(before: &Campaign, after: &Campaign) -> Vec<String> {
    RECONCILED_FIELDS
        .iter()
        .filter(|field| field_value(before, field) != field_value(after, field))
        .map(|field| (*field).to_string())
        .collect()
}

fn field_value(campaign: &Campaign, field: &str) -> serde_json::Value {
    match field {
        "name" => serde_json::json!(campaign.name),
        "objective" => serde_json::json!(campaign.objective),
        "status" => serde_json::json!(campaign.status),
        "budget" => serde_json::json!(campaign.budget),
        "schedule" => serde_json::json!(campaign.schedule),
        "ad_groups" => serde_json::json!(campaign.ad_groups),
        _ => unreachable!("reconciled fields are fixed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Budget, BudgetType, CampaignStatus, DriftState, Money, Objective};

    fn test_campaign() -> Campaign {
        Campaign {
            id: "campaign-1".to_string(),
            name: "Campaign".to_string(),
            status: CampaignStatus::Draft,
            objective: Objective::parse("sales").unwrap(),
            budget: Budget {
                kind: BudgetType::Daily,
                amount: Money::new(100, "JPY").unwrap(),
            },
            schedule: None,
            ad_groups: Vec::new(),
            drift: DriftState {
                drifted: false,
                changed_fields: Vec::new(),
            },
        }
    }

    #[test]
    fn unchanged_remote_recomputes_stale_drift_after_reapply() {
        let original = test_campaign();
        let mut state = CampaignReconciliationState::new(original.clone());
        state.intent.drift = DriftState {
            drifted: true,
            changed_fields: vec!["budget".to_string()],
        };
        assert_eq!(state.reconcile(original), ReconciliationResult::Unchanged);
        assert!(!state.intent.drift.drifted);
        assert!(state.intent.drift.changed_fields.is_empty());
    }

    #[test]
    fn unchanged_remote_preserves_a_real_conflict() {
        let mut state = CampaignReconciliationState::new(test_campaign());
        state.intent.budget.amount.amount_minor = 500;
        let remote = state.last_known_remote.clone();
        assert_eq!(state.reconcile(remote), ReconciliationResult::Unchanged);
        assert_eq!(state.intent.drift.changed_fields, vec!["budget"]);
        assert!(state.intent.drift.drifted);
    }

    #[test]
    fn native_pause_is_drift_and_never_rewrites_remote() {
        let mut original = test_campaign();
        original.status = CampaignStatus::Active;
        let mut state = CampaignReconciliationState::new(original.clone());
        let mut remote = original;
        remote.status = CampaignStatus::Paused;

        assert_eq!(
            state.reconcile(remote.clone()),
            ReconciliationResult::Drifted {
                fields: vec!["status".to_string()]
            }
        );
        assert_eq!(state.last_known_remote, remote);
        assert!(state.intent.drift.drifted);

        state.keep_theirs();
        assert_eq!(state.intent.status, CampaignStatus::Paused);
        assert!(!state.intent.drift.drifted);
    }

    #[test]
    fn unrelated_platform_change_updates_snapshot_without_drift() {
        let original = test_campaign();
        let mut state = CampaignReconciliationState::new(original.clone());
        state.intent.name = "Edited on the platform".to_string();
        let mut remote = original;
        remote.name = "Edited on the platform".to_string();

        assert_eq!(
            state.reconcile(remote.clone()),
            ReconciliationResult::RemoteUpdated {
                remote_changes: vec!["name".to_string()]
            }
        );
        assert_eq!(state.last_known_remote, remote);
        assert!(!state.intent.drift.drifted);
    }

    #[test]
    fn reapply_only_clears_drift_after_successful_provider_write() {
        let mut state = CampaignReconciliationState::new(test_campaign());
        state.intent.name = "Sanvi intent".to_string();
        let mut remote = state.last_known_remote.clone();
        remote.name = "Native change".to_string();
        assert!(matches!(
            state.reconcile(remote),
            ReconciliationResult::Drifted { .. }
        ));

        state.record_reapplied_ours();
        assert_eq!(state.last_known_remote.name, "Sanvi intent");
        assert!(!state.intent.drift.drifted);
    }

    #[test]
    fn unrelated_remote_change_does_not_clear_existing_drift() {
        let mut original = test_campaign();
        original.status = CampaignStatus::Active;
        let mut state = CampaignReconciliationState::new(original.clone());
        let mut paused = original;
        paused.status = CampaignStatus::Paused;
        assert!(matches!(
            state.reconcile(paused.clone()),
            ReconciliationResult::Drifted { .. }
        ));

        state.intent.name = "Renamed on the platform".to_string();
        let mut renamed = paused;
        renamed.name = "Renamed on the platform".to_string();
        assert_eq!(
            state.reconcile(renamed),
            ReconciliationResult::Drifted {
                fields: vec!["status".to_string()]
            }
        );
        assert_eq!(state.intent.drift.changed_fields, vec!["status"]);
    }
}
