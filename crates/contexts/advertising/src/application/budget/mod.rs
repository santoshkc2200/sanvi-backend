//! Budget guardrails, spend alerts, spend status reporting, and rollover logic (TASK-017, slice 10.8).

pub mod alerts;
pub mod caps;
pub mod enforcement;
pub mod evaluator;
pub mod rollover;
pub mod status;

pub use alerts::{AcknowledgeBudgetAlert, ListBudgetAlerts};
pub use caps::{
    DryRunEvaluationResult, GetBudgetCaps, GetCampaignBudgetCap, PutBudgetCap, PutBudgetCapCommand,
    PutBudgetCapResponse,
};
pub use enforcement::{EnforcementGate, SuspensionReason};
pub use evaluator::{SpendAnomalyDetector, ThresholdEvaluator};
pub use rollover::BudgetRolloverSweep;
pub use status::{CalculatedSpendDetails, GetSpendStatus, SpendStatusReport};
