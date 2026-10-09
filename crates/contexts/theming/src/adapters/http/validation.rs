//! TASK-027 (FR-1127): field addressing for the theming context's
//! validation errors.
//!
//! [`validation_issue`] classifies every variant of [`ThemingError`]:
//! `Some((detail, issues))` for the variants caused by an invalid input
//! value — `detail` byte-identical to what the flat mapping emitted — and
//! `None` for every other class. The match is exhaustive;
//! [`error_samples`] feeds the contract enumeration gate
//! (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::ThemingError;

pub fn validation_issue(err: &ThemingError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    match err {
        ThemingError::ThemeApiMismatch { found, supported } => addressed(
            "manifest",
            &format!("theme api {found} is not compatible with {supported}"),
        ),
        ThemingError::InvalidManifest(_) => addressed("manifest", err_text(err)),
        ThemingError::InvalidTokens(_) => addressed("tokens", err_text(err)),
        ThemingError::OverrideNotAllowed { path } => {
            addressed(&format!("token_overrides.{path}"), path)
        }
        ThemingError::OverrideTypeMismatch { path, .. } => {
            addressed(&format!("token_overrides.{path}"), path)
        }
        ThemingError::LayoutOverrideNotAllowed(_) => addressed("layout_overrides", err_text(err)),
        ThemingError::InvalidCustomCss(_) => addressed("custom_css", err_text(err)),
        ThemingError::ContrastFailure(_) => addressed("tokens", err_text(err)),
        ThemingError::InvalidBrandAsset(_) => addressed("asset", err_text(err)),
        ThemingError::UnknownTheme(_)
        | ThemingError::UnknownThemeVersion { .. }
        | ThemingError::ThemeVersionYanked { .. }
        | ThemingError::ThemeVersionExists { .. }
        | ThemingError::ThemeDeprecated(_)
        | ThemingError::ThemeNotEntitled { .. }
        | ThemingError::LayoutOverrideNotEntitled(_)
        | ThemingError::CustomCssNotEntitled(_)
        | ThemingError::InvalidState(_)
        | ThemingError::NothingToRollBack
        | ThemingError::InvalidPreviewToken
        | ThemingError::PreviewTokenExpired
        | ThemingError::Storage(_)
        | ThemingError::Cache(_)
        | ThemingError::Outbox(_)
        | ThemingError::IdGeneration(_) => None,
    }
}

/// The flat detail the mapping emits today: the variant's inner payload,
/// not its `Display` (which adds the surrounding prose).
fn err_text(err: &ThemingError) -> &str {
    match err {
        ThemingError::InvalidManifest(detail)
        | ThemingError::InvalidTokens(detail)
        | ThemingError::LayoutOverrideNotAllowed(detail)
        | ThemingError::InvalidCustomCss(detail)
        | ThemingError::ContrastFailure(detail)
        | ThemingError::InvalidBrandAsset(detail) => detail,
        ThemingError::OverrideNotAllowed { path }
        | ThemingError::OverrideTypeMismatch { path, .. } => path,
        _ => unreachable!("err_text is only called on classified variants"),
    }
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, ThemingError)> {
    vec![
        (
            "UnknownTheme",
            ThemingError::UnknownTheme("acme".to_string()),
        ),
        (
            "UnknownThemeVersion",
            ThemingError::UnknownThemeVersion {
                theme: "acme".to_string(),
                version: "9".to_string(),
            },
        ),
        (
            "ThemeVersionYanked",
            ThemingError::ThemeVersionYanked {
                theme: "acme".to_string(),
                version: "9".to_string(),
            },
        ),
        (
            "ThemeVersionExists",
            ThemingError::ThemeVersionExists {
                theme: "acme".to_string(),
                version: "9".to_string(),
            },
        ),
        (
            "ThemeDeprecated",
            ThemingError::ThemeDeprecated("acme".to_string()),
        ),
        (
            "ThemeNotEntitled",
            ThemingError::ThemeNotEntitled {
                feature: "theming.custom_css".to_string(),
            },
        ),
        (
            "ThemeApiMismatch",
            ThemingError::ThemeApiMismatch {
                found: "2".to_string(),
                supported: "^1".to_string(),
            },
        ),
        (
            "InvalidManifest",
            ThemingError::InvalidManifest("missing name".to_string()),
        ),
        (
            "InvalidTokens",
            ThemingError::InvalidTokens("unknown token".to_string()),
        ),
        (
            "OverrideNotAllowed",
            ThemingError::OverrideNotAllowed {
                path: "color.brand.primary".to_string(),
            },
        ),
        (
            "OverrideTypeMismatch",
            ThemingError::OverrideTypeMismatch {
                path: "color.brand.primary".to_string(),
                expected: "color".to_string(),
                found: "dimension".to_string(),
            },
        ),
        (
            "LayoutOverrideNotAllowed",
            ThemingError::LayoutOverrideNotAllowed("header".to_string()),
        ),
        (
            "LayoutOverrideNotEntitled",
            ThemingError::LayoutOverrideNotEntitled("theming.layout".to_string()),
        ),
        (
            "CustomCssNotEntitled",
            ThemingError::CustomCssNotEntitled("theming.custom_css".to_string()),
        ),
        (
            "InvalidCustomCss",
            ThemingError::InvalidCustomCss("unbalanced brace".to_string()),
        ),
        (
            "ContrastFailure",
            ThemingError::ContrastFailure("color.brand.primary".to_string()),
        ),
        (
            "InvalidState",
            ThemingError::InvalidState("draft".to_string()),
        ),
        ("NothingToRollBack", ThemingError::NothingToRollBack),
        ("InvalidPreviewToken", ThemingError::InvalidPreviewToken),
        ("PreviewTokenExpired", ThemingError::PreviewTokenExpired),
        (
            "InvalidBrandAsset",
            ThemingError::InvalidBrandAsset("not a png".to_string()),
        ),
        ("Storage", ThemingError::Storage("db down".to_string())),
        ("Cache", ThemingError::Cache("miss".to_string())),
        ("Outbox", ThemingError::Outbox("full".to_string())),
        (
            "IdGeneration",
            ThemingError::IdGeneration("generator lost".to_string()),
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
    fn token_override_paths_address_the_offending_entry() {
        let err = ThemingError::OverrideTypeMismatch {
            path: "color.brand.primary".to_string(),
            expected: "color".to_string(),
            found: "dimension".to_string(),
        };
        let (detail, issues) = validation_issue(&err).expect("validation");
        assert_eq!(detail, "color.brand.primary");
        assert_eq!(issues[0].field, "token_overrides.color.brand.primary");
    }

    #[test]
    fn contrast_failures_belong_to_the_token_document() {
        let err = ThemingError::ContrastFailure("color.text/body".to_string());
        let (_, issues) = validation_issue(&err).expect("validation");
        assert_eq!(issues[0].field, "tokens");
    }
}
