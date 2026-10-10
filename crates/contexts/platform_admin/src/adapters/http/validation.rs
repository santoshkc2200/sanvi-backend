//! TASK-027 (FR-1127): field addressing for the platform-admin context's
//! validation errors. [`validation_issue`] classifies every variant of
//! [`PlatformAdminError`] — `Some((detail, issues))` for invalid-input
//! shapes (the flat mapping emits the `Display` as `detail` for these),
//! `None` otherwise, the match exhaustive. [`error_samples`] feeds the
//! contract enumeration gate (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::{ApprovalId, ImpersonationGrantId, PlatformAdminError};

pub fn validation_issue(err: &PlatformAdminError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, err: &PlatformAdminError| {
        Some(FieldIssue::addressing(field, &err.to_string()))
    };
    match err {
        PlatformAdminError::MissingReason => addressed("reason", err),
        PlatformAdminError::DurationTooLong { .. } => addressed("duration_minutes", err),
        PlatformAdminError::InvalidDuration => addressed("duration_minutes", err),
        PlatformAdminError::InvalidActionType(_) => addressed("action", err),
        PlatformAdminError::InvalidSearch(_) => addressed("query", err),
        PlatformAdminError::InvalidEntitlement(_) => addressed("entitlements", err),
        PlatformAdminError::InvalidFeeConfig(_) => addressed("application_fee", err),
        PlatformAdminError::GrantNotFound { .. }
        | PlatformAdminError::GrantRevoked { .. }
        | PlatformAdminError::GrantExpired { .. }
        | PlatformAdminError::TargetNotMember { .. }
        | PlatformAdminError::PrivacyLocked { .. }
        | PlatformAdminError::TenantNotFound { .. }
        | PlatformAdminError::OffboardingConflict(_)
        | PlatformAdminError::OffboardingExportMissing(_)
        | PlatformAdminError::ApprovalNotFound { .. }
        | PlatformAdminError::ApprovalNotPending { .. }
        | PlatformAdminError::SelfApproval
        | PlatformAdminError::FeatureNotFound(_)
        | PlatformAdminError::EntitlementConflict(_)
        | PlatformAdminError::Storage(_)
        | PlatformAdminError::IdGeneration(_) => None,
    }
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, PlatformAdminError)> {
    vec![
        ("MissingReason", PlatformAdminError::MissingReason),
        (
            "DurationTooLong",
            PlatformAdminError::DurationTooLong { max_minutes: 60 },
        ),
        ("InvalidDuration", PlatformAdminError::InvalidDuration),
        (
            "GrantNotFound",
            PlatformAdminError::GrantNotFound {
                id: ImpersonationGrantId::try_from(910_951_001_i64).expect("fixed id"),
            },
        ),
        (
            "GrantRevoked",
            PlatformAdminError::GrantRevoked {
                id: ImpersonationGrantId::try_from(910_951_001_i64).expect("fixed id"),
            },
        ),
        (
            "GrantExpired",
            PlatformAdminError::GrantExpired {
                id: ImpersonationGrantId::try_from(910_951_001_i64).expect("fixed id"),
            },
        ),
        (
            "TargetNotMember",
            PlatformAdminError::TargetNotMember {
                user_id: sanvi_kernel::UserId::try_from(910_951_002_i64).expect("fixed id"),
                tenant_id: sanvi_kernel::TenantId::try_from(910_951_003_i64).expect("fixed id"),
            },
        ),
        (
            "PrivacyLocked",
            PlatformAdminError::PrivacyLocked {
                tenant_id: sanvi_kernel::TenantId::try_from(910_951_003_i64).expect("fixed id"),
            },
        ),
        (
            "TenantNotFound",
            PlatformAdminError::TenantNotFound {
                id: sanvi_kernel::TenantId::try_from(910_951_003_i64).expect("fixed id"),
            },
        ),
        (
            "ApprovalNotFound",
            PlatformAdminError::ApprovalNotFound {
                id: ApprovalId::try_from(910_951_004_i64).expect("fixed id"),
            },
        ),
        (
            "ApprovalNotPending",
            PlatformAdminError::ApprovalNotPending {
                id: ApprovalId::try_from(910_951_004_i64).expect("fixed id"),
            },
        ),
        ("SelfApproval", PlatformAdminError::SelfApproval),
        (
            "InvalidActionType",
            PlatformAdminError::InvalidActionType("vibes".to_string()),
        ),
        (
            "InvalidEntitlement",
            PlatformAdminError::InvalidEntitlement("no such feature".to_string()),
        ),
        (
            "FeatureNotFound",
            PlatformAdminError::FeatureNotFound("advertising".to_string()),
        ),
        (
            "EntitlementConflict",
            PlatformAdminError::EntitlementConflict("plan grant wins".to_string()),
        ),
        (
            "InvalidSearch",
            PlatformAdminError::InvalidSearch("query too short".to_string()),
        ),
        (
            "InvalidFeeConfig",
            PlatformAdminError::InvalidFeeConfig("negative rate".to_string()),
        ),
        (
            "Storage",
            PlatformAdminError::Storage("db down".to_string()),
        ),
        (
            "IdGeneration",
            PlatformAdminError::IdGeneration("lost".to_string()),
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
    fn impersonation_input_rejections_name_their_field() {
        let (_, issues) = validation_issue(&PlatformAdminError::MissingReason).expect("reason");
        assert_eq!(issues[0].field, "reason");
        let duration = PlatformAdminError::DurationTooLong { max_minutes: 60 };
        let (_, issues) = validation_issue(&duration).expect("duration");
        assert_eq!(issues[0].field, "duration_minutes");
        assert_eq!(
            issues[0].message,
            "impersonation duration exceeds the maximum of 60 minutes"
        );
    }
}
