//! Capability and requirement value objects: the provider-agnostic
//! projection of "what this connected account may do" and "what is
//! outstanding before it may do more".
//!
//! Providers persist their own verbatim JSON on the connection
//! (`capabilities`, `requirements` columns — 09.0's contract); these
//! types are the mapped, typed view over that JSON. The one mapping rule
//! that matters: go-live reads the `card_payments` capability status,
//! never a deprecated boolean.

use chrono::{DateTime, Utc};

#[cfg(test)]
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The lifecycle of one provider capability. Mirrors Stripe v2 capability
/// statuses without naming Stripe: `active` (usable), `pending` (requested,
/// not yet granted), `inactive` (revoked or never requested).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Active,
    Pending,
    Inactive,
}

impl CapabilityStatus {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "active" => Self::Active,
            "pending" => Self::Pending,
            "inactive" => Self::Inactive,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Inactive => "inactive",
        }
    }
}

/// A provider's capability map. `card_payments` is the go-live capability
/// for this release; other entries (transfers, LPMs) ride along untouched.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CapabilitySet {
    capabilities: std::collections::BTreeMap<String, CapabilityStatus>,
}

impl CapabilitySet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: &str, status: CapabilityStatus) -> Self {
        self.capabilities.insert(name.to_string(), status);
        self
    }

    pub fn get(&self, name: &str) -> Option<CapabilityStatus> {
        self.capabilities.get(name).copied()
    }

    /// The go-live check: `card_payments` present and `active`.
    pub fn card_payments(&self) -> Option<CapabilityStatus> {
        self.get("card_payments")
    }

    pub fn is_card_payments_active(&self) -> bool {
        self.card_payments() == Some(CapabilityStatus::Active)
    }

    /// Parse from provider JSON. Accepts the flat form
    /// `{"card_payments": "active"}` and the nested Stripe v2 form
    /// `{"card_payments": {"status": "active"}}`; unknown statuses become
    /// `Inactive` — never an error, because a provider adding a status
    /// must not take checkout down.
    pub fn from_provider_json(value: &Value) -> Self {
        let mut set = Self::empty();
        let Some(map) = value.as_object() else {
            return set;
        };
        for (name, raw) in map {
            let status = match raw {
                Value::String(s) => CapabilityStatus::parse(s),
                Value::Object(nested) => nested
                    .get("status")
                    .and_then(Value::as_str)
                    .and_then(CapabilityStatus::parse),
                _ => None,
            }
            .unwrap_or(CapabilityStatus::Inactive);
            set.capabilities.insert(name.clone(), status);
        }
        set
    }

    pub fn to_provider_json(&self) -> Value {
        serde_json::Map::from_iter(
            self.capabilities
                .iter()
                .map(|(name, status)| (name.clone(), Value::String(status.as_str().to_string()))),
        )
        .into()
    }

    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }
}

/// The provider's outstanding-requirements summary, projected. The tenant
/// sees exactly these lists; deadlines drive the 09.7 "stuck onboarding"
/// alert.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RequirementSummary {
    /// Fields the provider needs *now* (blocks activation).
    pub currently_due: Vec<String>,
    /// Fields the provider will need eventually (does not block).
    pub eventually_due: Vec<String>,
    /// Fields past their deadline — the account is restricted until
    /// resolved.
    pub past_due: Vec<String>,
    /// When `currently_due` becomes `past_due`.
    pub deadline: Option<DateTime<Utc>>,
}

impl RequirementSummary {
    pub fn is_empty(&self) -> bool {
        self.currently_due.is_empty() && self.eventually_due.is_empty() && self.past_due.is_empty()
    }

    /// Parse from provider JSON, tolerating both the flat array form
    /// (`{"currently_due": ["…"]}`) and empty/absent input. The deadline
    /// arrives as an epoch integer on provider wire formats and as an
    /// RFC-3339 string once round-tripped through storage — both parse.
    pub fn from_provider_json(value: &Value) -> Self {
        let strings = |key: &str| -> Vec<String> {
            value
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let deadline = match value.get("deadline") {
            Some(Value::Number(n)) => n
                .as_i64()
                .and_then(|secs| DateTime::from_timestamp(secs, 0)),
            Some(Value::String(raw)) => raw.parse::<DateTime<Utc>>().ok(),
            _ => None,
        };
        Self {
            currently_due: strings("currently_due"),
            eventually_due: strings("eventually_due"),
            past_due: strings("past_due"),
            deadline,
        }
    }
}

/// Parse a connection's stored capability JSON into the typed view. A
/// well-formed-but-unknown shape parses to defaults, never an error
/// (see [`CapabilitySet::from_provider_json`]).
pub fn capability_set(value: &Value) -> CapabilitySet {
    CapabilitySet::from_provider_json(value)
}

/// Parse a connection's stored requirements JSON into the typed view.
pub fn requirement_summary(value: &Value) -> RequirementSummary {
    RequirementSummary::from_provider_json(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_statuses_round_trip() {
        for status in [
            CapabilityStatus::Active,
            CapabilityStatus::Pending,
            CapabilityStatus::Inactive,
        ] {
            assert_eq!(CapabilityStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(CapabilityStatus::parse("disabled"), None);
    }

    #[test]
    fn card_payments_is_the_go_live_check() {
        let mut set = CapabilitySet::empty();
        assert!(!set.is_card_payments_active());
        assert_eq!(set.card_payments(), None);

        set = set.with("card_payments", CapabilityStatus::Pending);
        assert!(!set.is_card_payments_active());

        set = set.with("card_payments", CapabilityStatus::Active);
        assert!(set.is_card_payments_active());
    }

    #[test]
    fn capability_json_parses_flat_and_nested_forms() {
        let flat = CapabilitySet::from_provider_json(&serde_json::json!({
            "card_payments": "active",
            "transfers": "pending"
        }));
        assert!(flat.is_card_payments_active());
        assert_eq!(flat.get("transfers"), Some(CapabilityStatus::Pending));

        // Stripe v2 nests the status one level down.
        let nested = CapabilitySet::from_provider_json(&serde_json::json!({
            "card_payments": {"status": "active"}
        }));
        assert!(nested.is_card_payments_active());

        // Unknown statuses degrade to Inactive, never an error.
        let unknown = CapabilitySet::from_provider_json(&serde_json::json!({
            "card_payments": "weird_new_status"
        }));
        assert_eq!(unknown.card_payments(), Some(CapabilityStatus::Inactive));

        // Non-object JSON is empty, not a panic.
        assert!(CapabilitySet::from_provider_json(&Value::Null).is_empty());
    }

    #[test]
    fn requirement_summaries_parse_and_report_emptiness() {
        let empty = RequirementSummary::from_provider_json(&serde_json::json!({}));
        assert!(empty.is_empty());

        let due = RequirementSummary::from_provider_json(&serde_json::json!({
            "currently_due": ["business_profile.url"],
            "eventually_due": ["verification.document"],
            "past_due": [],
            "deadline": 1_800_000_000
        }));
        assert!(!due.is_empty());
        assert_eq!(due.currently_due, ["business_profile.url"]);
        assert_eq!(due.eventually_due, ["verification.document"]);
        assert!(due.deadline.is_some());
    }

    #[test]
    fn deadlines_round_trip_through_epoch_and_storage_forms() {
        // Provider wire form: epoch seconds.
        let wire = RequirementSummary::from_provider_json(&serde_json::json!({
            "deadline": 1_800_000_000
        }));
        assert_eq!(wire.deadline, DateTime::from_timestamp(1_800_000_000, 0));

        // Stored form (apply_status serializes the struct): RFC-3339.
        let stored = RequirementSummary::from_provider_json(&serde_json::json!({
            "deadline": "2027-01-15T00:00:00Z"
        }));
        assert_eq!(stored.deadline.unwrap().year(), 2027);

        // Garbage degrades to None, never an error.
        let garbage = RequirementSummary::from_provider_json(&serde_json::json!({
            "deadline": "soon-ish"
        }));
        assert_eq!(garbage.deadline, None);
    }
}
