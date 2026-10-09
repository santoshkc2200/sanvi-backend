//! TASK-027 (FR-1127): field addressing for the access context's
//! validation errors. [`validation_issue`] classifies every variant of
//! [`AccessError`] — `Some((detail, issues))` for invalid-input shapes,
//! `None` otherwise, the match exhaustive. [`error_samples`] feeds the
//! contract enumeration gate (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::AccessError;

pub fn validation_issue(err: &AccessError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    match err {
        AccessError::InvalidPermission(_) => addressed("permissions", err_text(err)),
        AccessError::InvalidRoleScope(_) => addressed("scope", err_text(err)),
        AccessError::InvalidRoleKey(_) => addressed("key", err_text(err)),
        AccessError::InvalidFeatureKey(_) => addressed("key", err_text(err)),
        AccessError::InvalidFeatureDefinition(detail) => feature_definition_issue(detail),
        AccessError::InvalidEntitlement(detail) => entitlement_issue(detail),
        AccessError::InvalidLimit(_) => addressed("limit", err_text(err)),
        AccessError::PermissionScopeMismatch { permission, scope } => addressed(
            "permissions",
            &format!("permission '{permission}' is not assignable in scope {scope}"),
        ),
        AccessError::UnknownPermission(_)
        | AccessError::RoleAlreadyExists { .. }
        | AccessError::RoleNotFound { .. }
        | AccessError::RoleKeyResolutionFailed(_)
        | AccessError::SystemRoleImmutable { .. }
        | AccessError::Escalation { .. }
        | AccessError::FeatureAlreadyExists { .. }
        | AccessError::FeatureNotFound { .. }
        | AccessError::EntitlementNotFound { .. }
        | AccessError::QuotaExceeded { .. }
        | AccessError::EntitlementBackendUnavailable(_)
        | AccessError::BackendUnreachable(_)
        | AccessError::BackendRejected(_)
        | AccessError::Storage(_)
        | AccessError::IdGeneration(_) => None,
    }
}

/// The flat detail the mapping emits today: the variant's inner string.
fn err_text(err: &AccessError) -> &str {
    match err {
        AccessError::InvalidPermission(detail)
        | AccessError::InvalidRoleScope(detail)
        | AccessError::InvalidRoleKey(detail)
        | AccessError::InvalidFeatureKey(detail)
        | AccessError::InvalidLimit(detail) => detail,
        _ => unreachable!("err_text is only called on classified variants"),
    }
}

/// `InvalidFeatureDefinition` covers several payload fields; the message's
/// leading phrase is the key.
fn feature_definition_issue(detail: &str) -> Option<(String, Vec<FieldIssue>)> {
    let field = if detail.starts_with("unknown feature kind") {
        "kind"
    } else if detail.starts_with("unknown visibility") {
        "visibility"
    } else if detail.starts_with("feature name") {
        "name"
    } else if detail.starts_with("boolean features cannot carry a default limit") {
        "default_limit"
    } else {
        return None;
    };
    Some(FieldIssue::addressing(field, detail))
}

/// `InvalidEntitlement` covers several payload fields; the message's
/// leading phrase is the key.
fn entitlement_issue(detail: &str) -> Option<(String, Vec<FieldIssue>)> {
    let field = if detail.starts_with("expires_at ") {
        "expires_at"
    } else if detail.starts_with("unknown entitlement source") {
        "source"
    } else {
        return None;
    };
    Some(FieldIssue::addressing(field, detail))
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, AccessError)> {
    vec![
        (
            "UnknownPermission",
            AccessError::UnknownPermission("tenant.read".to_string()),
        ),
        (
            "InvalidPermission",
            AccessError::InvalidPermission("bad key".to_string()),
        ),
        (
            "InvalidRoleScope",
            AccessError::InvalidRoleScope("galactic".to_string()),
        ),
        (
            "InvalidRoleKey",
            AccessError::InvalidRoleKey("bad key!".to_string()),
        ),
        (
            "RoleAlreadyExists",
            AccessError::RoleAlreadyExists {
                key: "editor".to_string(),
            },
        ),
        (
            "RoleNotFound",
            AccessError::RoleNotFound {
                key: "editor".to_string(),
            },
        ),
        (
            "RoleKeyResolutionFailed",
            AccessError::RoleKeyResolutionFailed(
                sanvi_kernel::RoleId::try_from(910_801_001_i64).expect("fixed id"),
            ),
        ),
        (
            "SystemRoleImmutable",
            AccessError::SystemRoleImmutable {
                key: "tenant_admin".to_string(),
            },
        ),
        (
            "PermissionScopeMismatch",
            AccessError::PermissionScopeMismatch {
                permission: "platform.tenants.write".to_string(),
                scope: "tenant".to_string(),
            },
        ),
        (
            "Escalation",
            AccessError::Escalation {
                permission: "tenant.billing.write".to_string(),
            },
        ),
        (
            "InvalidFeatureKey",
            AccessError::InvalidFeatureKey("bad key!".to_string()),
        ),
        (
            "InvalidFeatureDefinition",
            AccessError::InvalidFeatureDefinition("unknown feature kind: magic".to_string()),
        ),
        (
            "FeatureAlreadyExists",
            AccessError::FeatureAlreadyExists {
                key: "advertising".to_string(),
            },
        ),
        (
            "FeatureNotFound",
            AccessError::FeatureNotFound {
                key: "advertising".to_string(),
            },
        ),
        (
            "InvalidEntitlement",
            AccessError::InvalidEntitlement("expires_at must be in the future".to_string()),
        ),
        ("InvalidLimit", AccessError::InvalidLimit("-1".to_string())),
        (
            "EntitlementNotFound",
            AccessError::EntitlementNotFound {
                feature: "advertising".to_string(),
            },
        ),
        (
            "QuotaExceeded",
            AccessError::QuotaExceeded {
                feature: "seats".to_string(),
                limit: 10,
                attempted: 11,
            },
        ),
        (
            "EntitlementBackendUnavailable",
            AccessError::EntitlementBackendUnavailable("timeout".to_string()),
        ),
        (
            "BackendUnreachable",
            AccessError::BackendUnreachable("keto down".to_string()),
        ),
        (
            "BackendRejected",
            AccessError::BackendRejected("400".to_string()),
        ),
        ("Storage", AccessError::Storage("db down".to_string())),
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

    /// The grab-bag variants key their field on the message's leading
    /// phrase; every known message is pinned here.
    #[test]
    fn grab_bag_messages_stay_pinned() {
        let definition_cases: &[(&str, &str)] = &[
            ("unknown feature kind: magic", "kind"),
            ("unknown visibility: secret", "visibility"),
            ("feature name must not be empty", "name"),
            (
                "boolean features cannot carry a default limit",
                "default_limit",
            ),
        ];
        for (message, field) in definition_cases {
            let err = AccessError::InvalidFeatureDefinition((*message).to_string());
            let (_, issues) = validation_issue(&err).unwrap_or_else(|| panic!("{message}"));
            assert_eq!(issues[0].field, *field, "{message}");
        }
        let entitlement_cases: &[(&str, &str)] = &[
            ("expires_at must be in the future", "expires_at"),
            ("unknown entitlement source: moon", "source"),
        ];
        for (message, field) in entitlement_cases {
            let err = AccessError::InvalidEntitlement((*message).to_string());
            let (_, issues) = validation_issue(&err).unwrap_or_else(|| panic!("{message}"));
            assert_eq!(issues[0].field, *field, "{message}");
        }
        // An unknown entitlement message stays flat rather than guessing.
        let unknown = AccessError::InvalidEntitlement("something else".to_string());
        assert!(validation_issue(&unknown).is_none());
    }

    #[test]
    fn permission_scope_mismatch_names_the_permission_list() {
        let err = AccessError::PermissionScopeMismatch {
            permission: "platform.tenants.write".to_string(),
            scope: "tenant".to_string(),
        };
        let (detail, issues) = validation_issue(&err).expect("validation");
        assert_eq!(
            detail,
            "permission 'platform.tenants.write' is not assignable in scope tenant"
        );
        assert_eq!(issues[0].field, "permissions");
    }
}
