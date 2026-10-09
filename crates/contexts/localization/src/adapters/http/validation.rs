//! TASK-027 (FR-1127): field addressing for the localization context's
//! validation errors. [`validation_issue`] classifies every variant of
//! [`LocalizationError`] — `Some((detail, issues))` for invalid-input
//! shapes, `None` otherwise, the match exhaustive. [`error_samples`] feeds
//! the contract enumeration gate
//! (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::LocalizationError;

pub fn validation_issue(err: &LocalizationError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    match err {
        LocalizationError::InvalidLocale(_) => addressed("locale", err_text(err)),
        // The only construction site validates the settings command's
        // enabled set.
        LocalizationError::UnsupportedLocale(detail) => addressed(
            "enabled_locales",
            &format!("locale '{detail}' is not supported on this platform"),
        ),
        LocalizationError::InvalidTimezone(_) => addressed("timezone", err_text(err)),
        LocalizationError::InvalidOverride {
            key,
            locale,
            detail,
        } => Some((
            detail.clone(),
            vec![FieldIssue::new(
                format!("overrides.{key}.{locale}"),
                detail.clone(),
            )],
        )),
        LocalizationError::InvalidPattern { key, detail } => {
            let message = format!("invalid message pattern for '{key}': {detail}");
            Some((
                message.clone(),
                vec![FieldIssue::new(format!("overrides.{key}"), message)],
            ))
        }
        LocalizationError::UnknownMessageKey(detail) => {
            addressed("key", &format!("unknown message key '{detail}'"))
        }
        LocalizationError::InvalidAddress(_) => addressed("address", err_text(err)),
        LocalizationError::UnknownDomain(_)
        | LocalizationError::UnknownCurrency(_)
        | LocalizationError::SettingsConflict(_)
        | LocalizationError::Storage(_)
        | LocalizationError::Outbox(_) => None,
    }
}

/// The flat detail the mapping emits today: the variant's inner string.
fn err_text(err: &LocalizationError) -> &str {
    match err {
        LocalizationError::InvalidLocale(detail)
        | LocalizationError::InvalidTimezone(detail)
        | LocalizationError::InvalidAddress(detail) => detail,
        _ => unreachable!("err_text is only called on classified variants"),
    }
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, LocalizationError)> {
    vec![
        (
            "InvalidLocale",
            LocalizationError::InvalidLocale("xx".to_string()),
        ),
        (
            "UnsupportedLocale",
            LocalizationError::UnsupportedLocale("xx".to_string()),
        ),
        (
            "InvalidTimezone",
            LocalizationError::InvalidTimezone("Mars/Olympus".to_string()),
        ),
        (
            "UnknownDomain",
            LocalizationError::UnknownDomain("nosuch".to_string()),
        ),
        (
            "UnknownMessageKey",
            LocalizationError::UnknownMessageKey("emails.nosuch".to_string()),
        ),
        (
            "InvalidOverride",
            LocalizationError::InvalidOverride {
                key: "emails.invitation.body".to_string(),
                locale: "xx".to_string(),
                detail: "unbalanced placeholder".to_string(),
            },
        ),
        (
            "InvalidPattern",
            LocalizationError::InvalidPattern {
                key: "emails.invitation.body".to_string(),
                detail: "unbalanced placeholder".to_string(),
            },
        ),
        (
            "UnknownCurrency",
            LocalizationError::UnknownCurrency("XXXQ".to_string()),
        ),
        (
            "InvalidAddress",
            LocalizationError::InvalidAddress("no lines".to_string()),
        ),
        (
            "SettingsConflict",
            LocalizationError::SettingsConflict(
                "default locale xx is not in the enabled set".to_string(),
            ),
        ),
        ("Storage", LocalizationError::Storage("db down".to_string())),
        ("Outbox", LocalizationError::Outbox("full".to_string())),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_shapes_carry_fields_non_validation_shapes_do_not() {
        for (name, err) in error_samples() {
            match validation_issue(&err) {
                Some((detail, issues)) => {
                    assert!(
                        !issues.is_empty(),
                        "{name} classifies as validation but has no field"
                    );
                    for issue in issues {
                        assert!(
                            !issue.field.is_empty(),
                            "{name} carries an empty field path"
                        );
                        assert_eq!(
                            issue.message, detail,
                            "{name} must not rewrite the flat message when addressing it"
                        );
                    }
                }
                None => continue,
            }
        }
    }

    #[test]
    fn pattern_rejections_address_the_override_entry() {
        let err = LocalizationError::InvalidPattern {
            key: "emails.invitation.body".to_string(),
            detail: "unbalanced placeholder".to_string(),
        };
        let (detail, issues) = validation_issue(&err).expect("validation");
        assert_eq!(
            detail,
            "invalid message pattern for 'emails.invitation.body': unbalanced placeholder"
        );
        assert_eq!(issues[0].field, "overrides.emails.invitation.body");
    }
}
