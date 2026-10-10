//! TASK-027 (FR-1127): field addressing for the tenancy context's
//! validation errors.
//!
//! [`validation_issue`] classifies every variant of [`TenancyError`]:
//! `Some((detail, issues))` for the variants caused by an invalid input
//! value — `detail` byte-identical to what the flat mapping emitted — and
//! `None` for every other class. The match is exhaustive; [`error_samples`]
//! feeds the contract enumeration gate
//! (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::TenancyError;

pub fn validation_issue(err: &TenancyError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    match err {
        TenancyError::InvalidSlug(detail) => addressed("slug", detail),
        TenancyError::InvalidDisplayName(detail) => addressed("display_name", detail),
        TenancyError::InvalidRegion(detail) => addressed("region", detail),
        TenancyError::InvalidLocale(detail) => addressed("locale", detail),
        TenancyError::InvalidSuspensionReason(detail) => addressed("reason", detail),
        TenancyError::InvalidSettings(detail) => addressed("settings", detail),
        TenancyError::SlugReserved { .. }
        | TenancyError::SlugTaken { .. }
        | TenancyError::TenantNotFound { .. }
        | TenancyError::IllegalTransition { .. }
        | TenancyError::OffboardingTransition { .. }
        | TenancyError::OffboardingExportMissing { .. }
        | TenancyError::Conflict(_)
        | TenancyError::MissingTenantContext
        | TenancyError::InvalidStatus(_)
        | TenancyError::Storage(_)
        | TenancyError::IdGeneration(_) => None,
    }
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, TenancyError)> {
    vec![
        ("InvalidSlug", TenancyError::InvalidSlug("-bad".to_string())),
        (
            "SlugReserved",
            TenancyError::SlugReserved {
                slug: "admin".to_string(),
            },
        ),
        (
            "SlugTaken",
            TenancyError::SlugTaken {
                slug: "acme".to_string(),
            },
        ),
        (
            "InvalidDisplayName",
            TenancyError::InvalidDisplayName("bad name".to_string()),
        ),
        (
            "InvalidRegion",
            TenancyError::InvalidRegion("mars".to_string()),
        ),
        (
            "InvalidLocale",
            TenancyError::InvalidLocale("xx".to_string()),
        ),
        (
            "TenantNotFound",
            TenancyError::TenantNotFound {
                id: "910_601_001".to_string(),
            },
        ),
        (
            "IllegalTransition",
            TenancyError::IllegalTransition {
                from: "active",
                to: "provisioning",
            },
        ),
        ("MissingTenantContext", TenancyError::MissingTenantContext),
        ("Storage", TenancyError::Storage("db down".to_string())),
        (
            "InvalidSuspensionReason",
            TenancyError::InvalidSuspensionReason("because".to_string()),
        ),
        (
            "InvalidStatus",
            TenancyError::InvalidStatus("vaporised".to_string()),
        ),
        (
            "InvalidSettings",
            TenancyError::InvalidSettings("settings must not be empty".to_string()),
        ),
        (
            "IdGeneration",
            TenancyError::IdGeneration("generator lost".to_string()),
        ),
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
    fn input_fields_name_their_request_field() {
        let cases: &[(&str, TenancyError, &str)] = &[
            (
                "InvalidSlug",
                TenancyError::InvalidSlug("-x".into()),
                "slug",
            ),
            (
                "InvalidDisplayName",
                TenancyError::InvalidDisplayName(String::new()),
                "display_name",
            ),
            (
                "InvalidRegion",
                TenancyError::InvalidRegion("mars".into()),
                "region",
            ),
            (
                "InvalidLocale",
                TenancyError::InvalidLocale("xx".into()),
                "locale",
            ),
            (
                "InvalidSettings",
                TenancyError::InvalidSettings("settings must not be empty".into()),
                "settings",
            ),
        ];
        for (name, err, field) in cases {
            let (_, issues) = validation_issue(err).expect(name);
            assert_eq!(issues[0].field, *field, "{name}");
        }
        // Slug conflicts are availability, not input shape: no field.
        let taken = TenancyError::SlugTaken {
            slug: "acme".to_string(),
        };
        assert!(validation_issue(&taken).is_none());
    }
}
