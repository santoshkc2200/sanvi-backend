//! TASK-027 (FR-1127): field addressing for the advertising context's
//! validation errors.
//!
//! The campaign create/update/validate paths already address fields — the
//! capability matrix raises [`ValidationViolation`]s whose `field_path`
//! reaches the client as the `advertising/campaign-invalid` problem's
//! `violations` member. [`validation_issue`] extends the same addressing
//! to the generic error mapping: `Validation` converts its violations to
//! field issues, and the input-shaped `InvalidInput` messages name their
//! field. The match is exhaustive; [`error_samples`] feeds the contract
//! enumeration gate (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::{AdvertisingError, ValidationViolation};

/// The flat rejection text plus the fields it addresses, or `None` when
/// the variant is not an invalid-input rejection.
pub fn validation_issue(err: &AdvertisingError) -> Option<(String, Vec<FieldIssue>)> {
    match err {
        AdvertisingError::Validation(violations) => {
            let issues: Vec<FieldIssue> = violations.iter().map(violation_to_issue).collect();
            (!issues.is_empty()).then(|| {
                (
                    format!(
                        "campaign validation failed with {} violation(s)",
                        issues.len()
                    ),
                    issues,
                )
            })
        }
        AdvertisingError::InvalidInput(detail) => invalid_input_issue(detail),
        AdvertisingError::PlatformUnavailable(_)
        | AdvertisingError::Storage(_)
        | AdvertisingError::StaleRevision
        | AdvertisingError::CampaignNotFound
        | AdvertisingError::ConnectionNotActive
        | AdvertisingError::ConnectionNotFound
        | AdvertisingError::ConnectionTokenMissing
        | AdvertisingError::CampaignNotPublished
        | AdvertisingError::AdGroupNotFound
        | AdvertisingError::AdNotFound
        | AdvertisingError::CreativeNotFound
        | AdvertisingError::ConversionNotFound
        | AdvertisingError::AudienceNotFound
        | AdvertisingError::BudgetCapNotFound
        | AdvertisingError::BudgetAlertNotFound
        | AdvertisingError::CapBelowCurrentSpend
        | AdvertisingError::CapVersionRequired
        | AdvertisingError::EncryptionUnconfigured
        | AdvertisingError::EncryptionFailure(_)
        | AdvertisingError::IdGeneration(_) => None,
    }
}

fn violation_to_issue(violation: &ValidationViolation) -> FieldIssue {
    FieldIssue::new(violation.field_path.clone(), violation.message.clone())
}

/// `InvalidInput` serves several inputs; the message's leading phrase is
/// the key. Adapter-internal and auth-shaped messages (refresh tokens,
/// site keys, tracking secrets) are not request fields and stay
/// unclassified.
fn invalid_input_issue(detail: &str) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    if detail.starts_with("budget cap amount ") {
        addressed("amount_minor", detail)
    } else if detail.starts_with("audience name is required") {
        addressed("name", detail)
    } else if detail.starts_with("metrics query 'to' ") {
        addressed("to", detail)
    } else if detail.starts_with("metrics query spans ") {
        // The whole requested window is too wide: both ends name it.
        Some((
            detail.to_string(),
            vec![
                FieldIssue::new("from", detail.to_string()),
                FieldIssue::new("to", detail.to_string()),
            ],
        ))
    } else if detail.starts_with("advertising OAuth redirect_uri ") {
        addressed("redirect_uri", detail)
    } else if detail.starts_with("invalid creative id") {
        addressed("creative_id", detail)
    } else {
        None
    }
}

/// One constructed sample per variant, labelled with the variant name. The
/// contract enumeration gate walks this and fails on a validation shape
/// whose issue list is empty.
pub fn error_samples() -> Vec<(&'static str, AdvertisingError)> {
    vec![
        (
            "InvalidInput",
            AdvertisingError::InvalidInput(
                "budget cap amount must be strictly positive".to_string(),
            ),
        ),
        (
            "PlatformUnavailable",
            AdvertisingError::PlatformUnavailable("meta down".to_string()),
        ),
        ("Storage", AdvertisingError::Storage("db down".to_string())),
        ("StaleRevision", AdvertisingError::StaleRevision),
        ("CampaignNotFound", AdvertisingError::CampaignNotFound),
        (
            "Validation",
            AdvertisingError::Validation(vec![ValidationViolation::unsupported(
                "ad_groups[2].ads[0].creative.placement",
                "creative placement is unavailable for this platform",
            )]),
        ),
        ("ConnectionNotActive", AdvertisingError::ConnectionNotActive),
        ("ConnectionNotFound", AdvertisingError::ConnectionNotFound),
        (
            "ConnectionTokenMissing",
            AdvertisingError::ConnectionTokenMissing,
        ),
        (
            "CampaignNotPublished",
            AdvertisingError::CampaignNotPublished,
        ),
        ("AdGroupNotFound", AdvertisingError::AdGroupNotFound),
        ("AdNotFound", AdvertisingError::AdNotFound),
        ("CreativeNotFound", AdvertisingError::CreativeNotFound),
        ("ConversionNotFound", AdvertisingError::ConversionNotFound),
        ("AudienceNotFound", AdvertisingError::AudienceNotFound),
        ("BudgetCapNotFound", AdvertisingError::BudgetCapNotFound),
        ("BudgetAlertNotFound", AdvertisingError::BudgetAlertNotFound),
        (
            "CapBelowCurrentSpend",
            AdvertisingError::CapBelowCurrentSpend,
        ),
        ("CapVersionRequired", AdvertisingError::CapVersionRequired),
        (
            "EncryptionUnconfigured",
            AdvertisingError::EncryptionUnconfigured,
        ),
        (
            "EncryptionFailure",
            AdvertisingError::EncryptionFailure("hsm".to_string()),
        ),
        (
            "IdGeneration",
            AdvertisingError::IdGeneration("generator lost".to_string()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every validation shape names its field with a non-empty path, and
    /// the `Validation` variant carries one issue per violation.
    #[test]
    fn validation_shapes_carry_fields() {
        for (name, err) in error_samples() {
            if let Some((detail, issues)) = validation_issue(&err) {
                assert!(
                    !issues.is_empty(),
                    "{name} classifies as validation but has no field"
                );
                for issue in issues {
                    assert!(
                        !issue.field.is_empty(),
                        "{name} carries an empty field path"
                    );
                }
                if name == "Validation" {
                    assert!(
                        detail.contains("1 violation"),
                        "{name} detail names the count"
                    );
                }
            }
        }
    }

    #[test]
    fn wizard_paths_survive_into_field_issues() {
        let err = AdvertisingError::Validation(vec![ValidationViolation::invalid(
            "ad_groups[2].ads[0].creative.texts[1].headline",
            "headline exceeds the 30-character ja limit",
        )]);
        let (detail, issues) = validation_issue(&err).expect("validation");
        assert_eq!(detail, "campaign validation failed with 1 violation(s)");
        assert_eq!(
            issues[0].field,
            "ad_groups[2].ads[0].creative.texts[1].headline"
        );
    }

    #[test]
    fn input_message_prefixes_stay_pinned() {
        let cases: &[(&str, &str)] = &[
            (
                "budget cap amount must be strictly positive",
                "amount_minor",
            ),
            ("audience name is required", "name"),
            ("metrics query 'to' must not precede 'from'", "to"),
            ("advertising OAuth redirect_uri is required", "redirect_uri"),
            ("invalid creative id: parse error", "creative_id"),
        ];
        for (message, field) in cases {
            let err = AdvertisingError::InvalidInput((*message).to_string());
            let issues = validation_issue(&err).expect(message).1;
            assert_eq!(issues[0].field, *field, "message: {message}");
        }
        // A too-wide window names both ends.
        let err = AdvertisingError::InvalidInput(
            "metrics query spans 400 days, exceeding the maximum of 92".to_string(),
        );
        let issues = validation_issue(&err).expect("window").1;
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].field, "from");
        assert_eq!(issues[1].field, "to");
        // Adapter-internal shapes carry no field.
        let internal = AdvertisingError::InvalidInput("refresh token is required".to_string());
        assert!(validation_issue(&internal).is_none());
    }
}
