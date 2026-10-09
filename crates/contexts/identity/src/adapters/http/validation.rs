//! TASK-027 (FR-1127): field addressing for the identity context's
//! validation errors. [`validation_issue`] classifies every variant of
//! [`IdentityError`] — `Some((detail, issues))` for invalid-input shapes,
//! `None` otherwise, the match exhaustive. [`error_samples`] feeds the
//! contract enumeration gate (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::IdentityError;

pub fn validation_issue(err: &IdentityError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    match err {
        IdentityError::InvalidEmail(_) => addressed("email", err_text(err)),
        IdentityError::InvalidProvider(_) => addressed("provider", err_text(err)),
        IdentityError::InvalidLocale(_) => addressed("locale", err_text(err)),
        IdentityError::NoRoles => addressed("roles", "at least one role is required"),
        IdentityError::UnknownRole(id) => addressed("roles", &format!("unknown role id {id}")),
        IdentityError::RoleNotAssignable(id) => addressed(
            "roles",
            &format!("role {id} is not assignable in this tenant"),
        ),
        // Stored-status parses and webhook transport shapes are not
        // request-field rejections.
        IdentityError::InvalidUserStatus(_)
        | IdentityError::InvalidMembershipStatus(_)
        | IdentityError::MalformedWebhook(_)
        | IdentityError::UserNotFound { .. }
        | IdentityError::UserEmailNotFound { .. }
        | IdentityError::UserNotActive { .. }
        | IdentityError::IllegalUserTransition { .. }
        | IdentityError::NotAMember { .. }
        | IdentityError::CannotRemoveSelf
        | IdentityError::LastRoleHolder { .. }
        | IdentityError::AccountLinkedToOtherUser { .. }
        | IdentityError::AccountAlreadyLinked { .. }
        | IdentityError::InvitationNotFound { .. }
        | IdentityError::InvitationAlreadyAccepted
        | IdentityError::InvitationExpired
        | IdentityError::InvitationEmailMismatch
        | IdentityError::DelegationDenied { .. }
        | IdentityError::LinkIntentUnavailable
        | IdentityError::LinkIntentEmailMismatch
        | IdentityError::SessionNotFound
        | IdentityError::KratosUnreachable(_)
        | IdentityError::KratosRejected(_)
        | IdentityError::IdGeneration(_)
        | IdentityError::NotificationUnavailable(_)
        | IdentityError::Storage(_) => None,
    }
}

/// The flat detail the mapping emits today: the variant's inner string.
fn err_text(err: &IdentityError) -> &str {
    match err {
        IdentityError::InvalidEmail(detail)
        | IdentityError::InvalidProvider(detail)
        | IdentityError::InvalidLocale(detail) => detail,
        _ => unreachable!("err_text is only called on classified variants"),
    }
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, IdentityError)> {
    use crate::domain::user::UserId;
    use crate::domain::values::{Email, ProviderId};
    vec![
        (
            "InvalidEmail",
            IdentityError::InvalidEmail("not-an-email".to_string()),
        ),
        (
            "InvalidProvider",
            IdentityError::InvalidProvider("meta ads".to_string()),
        ),
        (
            "InvalidLocale",
            IdentityError::InvalidLocale("xx".to_string()),
        ),
        (
            "InvalidUserStatus",
            IdentityError::InvalidUserStatus("vaporised".to_string()),
        ),
        (
            "InvalidMembershipStatus",
            IdentityError::InvalidMembershipStatus("vaporised".to_string()),
        ),
        (
            "UserNotFound",
            IdentityError::UserNotFound {
                id: UserId::try_from(910_901_001_i64).expect("fixed id"),
            },
        ),
        (
            "UserEmailNotFound",
            IdentityError::UserEmailNotFound {
                email: Email::parse("nobody@example.com").expect("fixed email"),
            },
        ),
        (
            "UserNotActive",
            IdentityError::UserNotActive {
                id: UserId::try_from(910_901_001_i64).expect("fixed id"),
            },
        ),
        (
            "IllegalUserTransition",
            IdentityError::IllegalUserTransition {
                from: "active".to_string(),
                to: "provisioning".to_string(),
            },
        ),
        (
            "NotAMember",
            IdentityError::NotAMember {
                user_id: UserId::try_from(910_901_001_i64).expect("fixed id"),
                tenant_id: sanvi_kernel::TenantId::try_from(910_901_002_i64).expect("fixed id"),
            },
        ),
        ("CannotRemoveSelf", IdentityError::CannotRemoveSelf),
        (
            "LastRoleHolder",
            IdentityError::LastRoleHolder {
                from: crate::domain::MembershipStatus::Active,
                to: crate::domain::MembershipStatus::Suspended,
                role: "tenant_admin".to_string(),
            },
        ),
        (
            "AccountLinkedToOtherUser",
            IdentityError::AccountLinkedToOtherUser {
                provider: ProviderId::parse("google").expect("fixed provider"),
                subject: "sub-1".to_string(),
            },
        ),
        (
            "AccountAlreadyLinked",
            IdentityError::AccountAlreadyLinked {
                provider: ProviderId::parse("google").expect("fixed provider"),
            },
        ),
        (
            "InvitationNotFound",
            IdentityError::InvitationNotFound {
                id: crate::domain::InvitationId::try_from(910_901_003_i64).expect("fixed id"),
            },
        ),
        (
            "InvitationAlreadyAccepted",
            IdentityError::InvitationAlreadyAccepted,
        ),
        ("InvitationExpired", IdentityError::InvitationExpired),
        (
            "InvitationEmailMismatch",
            IdentityError::InvitationEmailMismatch,
        ),
        (
            "UnknownRole",
            IdentityError::UnknownRole(
                sanvi_kernel::RoleId::try_from(910_901_004_i64).expect("fixed id"),
            ),
        ),
        (
            "RoleNotAssignable",
            IdentityError::RoleNotAssignable(
                sanvi_kernel::RoleId::try_from(910_901_004_i64).expect("fixed id"),
            ),
        ),
        (
            "DelegationDenied",
            IdentityError::DelegationDenied {
                actor: UserId::try_from(910_901_001_i64).expect("fixed id"),
                role: "tenant_admin".to_string(),
                detail: "not a holder".to_string(),
            },
        ),
        ("NoRoles", IdentityError::NoRoles),
        (
            "LinkIntentUnavailable",
            IdentityError::LinkIntentUnavailable,
        ),
        (
            "LinkIntentEmailMismatch",
            IdentityError::LinkIntentEmailMismatch,
        ),
        ("SessionNotFound", IdentityError::SessionNotFound),
        (
            "MalformedWebhook",
            IdentityError::MalformedWebhook("no signature".to_string()),
        ),
        (
            "KratosUnreachable",
            IdentityError::KratosUnreachable("timeout".to_string()),
        ),
        (
            "KratosRejected",
            IdentityError::KratosRejected("400".to_string()),
        ),
        (
            "IdGeneration",
            IdentityError::IdGeneration("generator lost".to_string()),
        ),
        (
            "NotificationUnavailable",
            IdentityError::NotificationUnavailable("down".to_string()),
        ),
        ("Storage", IdentityError::Storage("db down".to_string())),
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
    fn role_rejections_address_the_roles_list() {
        for err in [
            IdentityError::NoRoles,
            IdentityError::UnknownRole(
                sanvi_kernel::RoleId::try_from(910_901_004_i64).expect("id"),
            ),
            IdentityError::RoleNotAssignable(
                sanvi_kernel::RoleId::try_from(910_901_004_i64).expect("id"),
            ),
        ] {
            let (_, issues) = validation_issue(&err).expect("role rejections are validation");
            assert_eq!(issues[0].field, "roles");
        }
        // Webhook transport rejections are not request-field errors.
        let webhook = IdentityError::MalformedWebhook("no signature".to_string());
        assert!(validation_issue(&webhook).is_none());
    }
}
