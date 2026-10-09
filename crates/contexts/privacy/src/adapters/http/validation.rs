//! TASK-027 (FR-1127): field addressing for the privacy context's
//! validation errors. [`validation_issue`] classifies every variant of
//! [`PrivacyError`] — `Some((detail, issues))` for invalid-input shapes
//! (the flat mapping emits the `Display` as `detail` for these), `None`
//! otherwise, the match exhaustive. [`error_samples`] feeds the contract
//! enumeration gate (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::PrivacyError;

pub fn validation_issue(err: &PrivacyError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed =
        |field: &str, err: &PrivacyError| Some(FieldIssue::addressing(field, &err.to_string()));
    match err {
        PrivacyError::InvalidJurisdiction(_) => addressed("jurisdiction", err),
        PrivacyError::InvalidRegime(_) => addressed("regime", err),
        PrivacyError::InvalidConsentModel(_) => addressed("consent_model", err),
        PrivacyError::InvalidSensitiveModel(_) => addressed("sensitive_model", err),
        PrivacyError::InvalidPurpose(_) => addressed("purpose", err),
        PrivacyError::InvalidDirectiveState(_) => addressed("state", err),
        PrivacyError::InvalidDirectiveSource(_) => addressed("source", err),
        PrivacyError::InvalidSensitivityClass(_) => addressed("sensitivity", err),
        PrivacyError::InvalidDataClass(_) => addressed("data_class", err),
        PrivacyError::InvalidRetentionAction(_) => addressed("action", err),
        PrivacyError::InvalidLegalBasis(_) => addressed("legal_basis", err),
        PrivacyError::InvalidSubjectKind(_) => addressed("kind", err),
        PrivacyError::InvalidDsrKind(_) => addressed("kind", err),
        PrivacyError::InvalidDsrStatus(_) => addressed("status", err),
        PrivacyError::InvalidRequester(_) => addressed("requester", err),
        PrivacyError::InvalidConsentMethod(_) => addressed("method", err),
        PrivacyError::InvalidSignalSource(_) => addressed("source", err),
        PrivacyError::InvalidSubProcessorRole(_) => addressed("role", err),
        PrivacyError::InvalidTransferMechanism(_) => addressed("transfer_mechanism", err),
        PrivacyError::InvalidObligationAudience(_) => addressed("audience", err),
        PrivacyError::InvalidCountryCode(_) => addressed("country_code", err),
        PrivacyError::AgentEvidenceMissing => Some(FieldIssue::addressing(
            "evidence",
            "authorized-agent submission requires the agent's identity and authorization evidence",
        )),
        // Consent semantics and state machine refusals carry no field: the
        // payload is well-formed, the situation is not.
        PrivacyError::AgentConsumerNotVerified
        | PrivacyError::SensitiveRequiresConsent
        | PrivacyError::MinorSubject(_)
        | PrivacyError::JurisdictionNotFound(_)
        | PrivacyError::SubjectNotFound(_)
        | PrivacyError::RequestNotFound(_)
        | PrivacyError::RequestNotOwned
        | PrivacyError::NotAppealable(_)
        | PrivacyError::AlreadyAppealed
        | PrivacyError::AppealNotFound(_)
        | PrivacyError::AppealSelfReview
        | PrivacyError::VerificationRequired
        | PrivacyError::ChallengeNotFound
        | PrivacyError::ChallengeExpired
        | PrivacyError::ChallengeExhausted
        | PrivacyError::ChallengeMismatch
        | PrivacyError::LegalHoldActive(_)
        | PrivacyError::EssentialNotConsentable
        | PrivacyError::KeyShredded
        | PrivacyError::EncryptionUnconfigured
        | PrivacyError::EncryptionFailure(_)
        | PrivacyError::ArtifactNotFound(_)
        | PrivacyError::ArtifactExpired
        | PrivacyError::ArtifactDownloadsExhausted
        | PrivacyError::ArtifactPassphraseMismatch
        | PrivacyError::PackagingFailed(_)
        | PrivacyError::Conflict(_)
        | PrivacyError::RateLimited(_)
        | PrivacyError::Upstream(_)
        | PrivacyError::IdGeneration(_)
        | PrivacyError::Storage(_) => None,
    }
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, PrivacyError)> {
    vec![
        (
            "InvalidJurisdiction",
            PrivacyError::InvalidJurisdiction("XX".to_string()),
        ),
        (
            "InvalidRegime",
            PrivacyError::InvalidRegime("magic".to_string()),
        ),
        (
            "InvalidConsentModel",
            PrivacyError::InvalidConsentModel("opt-sometimes".to_string()),
        ),
        (
            "InvalidSensitiveModel",
            PrivacyError::InvalidSensitiveModel("opt-sometimes".to_string()),
        ),
        (
            "InvalidPurpose",
            PrivacyError::InvalidPurpose("vibes".to_string()),
        ),
        (
            "InvalidDirectiveState",
            PrivacyError::InvalidDirectiveState("vaporised".to_string()),
        ),
        (
            "InvalidDirectiveSource",
            PrivacyError::InvalidDirectiveSource("dream".to_string()),
        ),
        (
            "InvalidSensitivityClass",
            PrivacyError::InvalidSensitivityClass("ultra".to_string()),
        ),
        (
            "InvalidDataClass",
            PrivacyError::InvalidDataClass("vibes".to_string()),
        ),
        (
            "InvalidRetentionAction",
            PrivacyError::InvalidRetentionAction("yeet".to_string()),
        ),
        (
            "InvalidLegalBasis",
            PrivacyError::InvalidLegalBasis("because".to_string()),
        ),
        (
            "InvalidSubjectKind",
            PrivacyError::InvalidSubjectKind("ghost".to_string()),
        ),
        (
            "InvalidDsrKind",
            PrivacyError::InvalidDsrKind("teleport".to_string()),
        ),
        (
            "InvalidDsrStatus",
            PrivacyError::InvalidDsrStatus("vibing".to_string()),
        ),
        (
            "InvalidRequester",
            PrivacyError::InvalidRequester("nobody".to_string()),
        ),
        (
            "InvalidConsentMethod",
            PrivacyError::InvalidConsentMethod("vibes".to_string()),
        ),
        (
            "InvalidSignalSource",
            PrivacyError::InvalidSignalSource("stars".to_string()),
        ),
        (
            "InvalidSubProcessorRole",
            PrivacyError::InvalidSubProcessorRole("intern".to_string()),
        ),
        (
            "InvalidTransferMechanism",
            PrivacyError::InvalidTransferMechanism("pigeon".to_string()),
        ),
        (
            "InvalidObligationAudience",
            PrivacyError::InvalidObligationAudience("everyone".to_string()),
        ),
        (
            "InvalidCountryCode",
            PrivacyError::InvalidCountryCode("XX1".to_string()),
        ),
        (
            "JurisdictionNotFound",
            PrivacyError::JurisdictionNotFound("XX".to_string()),
        ),
        (
            "SubjectNotFound",
            PrivacyError::SubjectNotFound("sub-1".to_string()),
        ),
        (
            "RequestNotFound",
            PrivacyError::RequestNotFound("req-1".to_string()),
        ),
        ("RequestNotOwned", PrivacyError::RequestNotOwned),
        (
            "NotAppealable",
            PrivacyError::NotAppealable("closed".to_string()),
        ),
        ("AlreadyAppealed", PrivacyError::AlreadyAppealed),
        (
            "AppealNotFound",
            PrivacyError::AppealNotFound("ap-1".to_string()),
        ),
        ("AppealSelfReview", PrivacyError::AppealSelfReview),
        ("VerificationRequired", PrivacyError::VerificationRequired),
        ("ChallengeNotFound", PrivacyError::ChallengeNotFound),
        ("ChallengeExpired", PrivacyError::ChallengeExpired),
        ("ChallengeExhausted", PrivacyError::ChallengeExhausted),
        ("ChallengeMismatch", PrivacyError::ChallengeMismatch),
        ("AgentEvidenceMissing", PrivacyError::AgentEvidenceMissing),
        (
            "AgentConsumerNotVerified",
            PrivacyError::AgentConsumerNotVerified,
        ),
        (
            "MinorSubject",
            PrivacyError::MinorSubject("dob".to_string()),
        ),
        (
            "SensitiveRequiresConsent",
            PrivacyError::SensitiveRequiresConsent,
        ),
        (
            "LegalHoldActive",
            PrivacyError::LegalHoldActive("case-1".to_string()),
        ),
        (
            "EssentialNotConsentable",
            PrivacyError::EssentialNotConsentable,
        ),
        ("KeyShredded", PrivacyError::KeyShredded),
        (
            "EncryptionUnconfigured",
            PrivacyError::EncryptionUnconfigured,
        ),
        (
            "EncryptionFailure",
            PrivacyError::EncryptionFailure("hsm".to_string()),
        ),
        (
            "ArtifactNotFound",
            PrivacyError::ArtifactNotFound("art-1".to_string()),
        ),
        ("ArtifactExpired", PrivacyError::ArtifactExpired),
        (
            "ArtifactDownloadsExhausted",
            PrivacyError::ArtifactDownloadsExhausted,
        ),
        (
            "ArtifactPassphraseMismatch",
            PrivacyError::ArtifactPassphraseMismatch,
        ),
        (
            "PackagingFailed",
            PrivacyError::PackagingFailed("zip".to_string()),
        ),
        ("Conflict", PrivacyError::Conflict("race".to_string())),
        (
            "RateLimited",
            PrivacyError::RateLimited("bucket".to_string()),
        ),
        ("Upstream", PrivacyError::Upstream("kratos".to_string())),
        (
            "IdGeneration",
            PrivacyError::IdGeneration("lost".to_string()),
        ),
        ("Storage", PrivacyError::Storage("db down".to_string())),
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
    fn jurisdiction_rejections_name_the_jurisdiction_field() {
        let err = PrivacyError::InvalidJurisdiction("XX".to_string());
        let (detail, issues) = validation_issue(&err).expect("validation");
        assert_eq!(detail, "invalid jurisdiction code: XX");
        assert_eq!(issues[0].field, "jurisdiction");
        // State semantics carry no field.
        let consent = PrivacyError::SensitiveRequiresConsent;
        assert!(validation_issue(&consent).is_none());
    }
}
