use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{BudgetType, Objective, TargetingDimension};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleGranularity {
    Daily,
    Hourly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreativePlacement {
    pub key: String,
    pub asset_spec: CreativeAssetSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreativeAssetSpec {
    pub image_required: bool,
    pub video_allowed: bool,
    pub aspect_ratios: Vec<String>,
}

/// Data consumed by both the validator and the frontend form engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CapabilityMatrix {
    pub matrix_version: String,
    pub objectives: BTreeSet<Objective>,
    pub budget_types: BTreeSet<BudgetType>,
    pub minimum_budgets_minor: BTreeMap<String, BTreeMap<BudgetType, i64>>,
    pub schedule_granularity: ScheduleGranularity,
    pub targeting_dimensions: BTreeSet<TargetingDimension>,
    pub creative_placements: Vec<CreativePlacement>,
    /// Locale -> field name -> inclusive character limit.
    pub text_limits: BTreeMap<String, BTreeMap<String, usize>>,
}

impl CapabilityMatrix {
    pub fn text_limit(&self, locale: &str, field: &str) -> Option<usize> {
        self.text_limits
            .get(locale)
            .or_else(|| self.text_limits.get("default"))
            .and_then(|limits| limits.get(field))
            .copied()
    }

    pub fn minimum_budget(&self, currency: &str, kind: BudgetType) -> Option<i64> {
        self.minimum_budgets_minor
            .get(currency)
            .and_then(|by_type| by_type.get(&kind))
            .copied()
    }

    pub fn placement(&self, key: &str) -> Option<&CreativePlacement> {
        self.creative_placements
            .iter()
            .find(|placement| placement.key == key)
    }
}
