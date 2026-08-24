use super::{Campaign, CapabilityMatrix};

/// Validate a draft exclusively from declarative matrix data.
pub fn validate_campaign(
    matrix: &CapabilityMatrix,
    campaign: &Campaign,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();
    if !matrix.objectives.contains(&campaign.objective) {
        violations.push(ValidationViolation::unsupported(
            "objective",
            "objective is unavailable for this platform",
        ));
    }
    if !matrix.budget_types.contains(&campaign.budget.kind) {
        violations.push(ValidationViolation::unsupported(
            "budget.kind",
            "budget type is unavailable for this platform",
        ));
    } else {
        match matrix.minimum_budget(&campaign.budget.amount.currency, campaign.budget.kind) {
            Some(minimum) if campaign.budget.amount.amount_minor < minimum => {
                violations.push(ValidationViolation::invalid(
                    "budget.amount.amount_minor",
                    format!("budget is below the {minimum} minor-unit minimum"),
                ))
            }
            None if !matrix
                .minimum_budgets_minor
                .contains_key(&campaign.budget.amount.currency) =>
            {
                violations.push(ValidationViolation::unsupported(
                    "budget.amount.currency",
                    "budget currency is unavailable for this platform",
                ));
            }
            None => violations.push(ValidationViolation::unsupported(
                "budget.amount.amount_minor",
                "budget minimum is unavailable for this platform",
            )),
            Some(_) => {}
        }
    }

    for (group_index, group) in campaign.ad_groups.iter().enumerate() {
        for dimension in group.targeting.dimensions.keys() {
            if !matrix.targeting_dimensions.contains(dimension) {
                violations.push(ValidationViolation::unsupported(
                    format!(
                        "ad_groups[{group_index}].targeting.dimensions.{}",
                        dimension.as_str()
                    ),
                    "targeting dimension is unavailable for this platform",
                ));
            }
        }
        for (ad_index, ad) in group.ads.iter().enumerate() {
            if matrix.placement(&ad.creative.placement).is_none() {
                violations.push(ValidationViolation::unsupported(
                    format!("ad_groups[{group_index}].ads[{ad_index}].creative.placement"),
                    "creative placement is unavailable for this platform",
                ));
            }
            for (text_index, text) in ad.creative.texts.iter().enumerate() {
                for (field, value) in [("headline", &text.headline), ("body", &text.body)] {
                    let field_path = format!(
                        "ad_groups[{group_index}].ads[{ad_index}].creative.texts[{text_index}].{field}"
                    );
                    match matrix.text_limit(&text.locale, field) {
                        Some(limit) if value.chars().count() > limit => {
                            violations.push(ValidationViolation::invalid(
                                field_path,
                                format!(
                                    "{field} exceeds the {limit}-character {} limit",
                                    text.locale
                                ),
                            ));
                        }
                        None => violations.push(ValidationViolation::unsupported(
                            field_path,
                            format!("{field} has no limit for locale {}", text.locale),
                        )),
                        Some(_) => {}
                    }
                }
            }
        }
    }
    violations
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ValidationViolation {
    pub field_path: String,
    pub code: String,
    pub message: String,
}

impl ValidationViolation {
    pub fn unsupported(field_path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field_path: field_path.into(),
            code: "capability_unsupported".to_string(),
            message: message.into(),
        }
    }

    pub fn invalid(field_path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field_path: field_path.into(),
            code: "invalid".to_string(),
            message: message.into(),
        }
    }
}
