//! Domain errors for the privacy context. This type never carries
//! infrastructure errors; storage failures are string-wrapped and mapped at
//! the adapters.

use thiserror::Error;

use sanvi_kernel::IdGenerationError;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PrivacyError {
    #[error("invalid jurisdiction code: {0}")]
    InvalidJurisdiction(String),

    #[error("invalid regime: {0}")]
    InvalidRegime(String),

    #[error("invalid consent model: {0}")]
    InvalidConsentModel(String),

    #[error("invalid sensitive model: {0}")]
    InvalidSensitiveModel(String),

    #[error("invalid processing purpose: {0}")]
    InvalidPurpose(String),

    #[error("invalid directive state: {0}")]
    InvalidDirectiveState(String),

    #[error("invalid directive source: {0}")]
    InvalidDirectiveSource(String),

    #[error("invalid sensitivity class: {0}")]
    InvalidSensitivityClass(String),

    #[error("invalid data class: {0}")]
    InvalidDataClass(String),

    #[error("invalid retention action: {0}")]
    InvalidRetentionAction(String),

    #[error("invalid legal basis: {0}")]
    InvalidLegalBasis(String),

    #[error("invalid subject kind: {0}")]
    InvalidSubjectKind(String),

    #[error("invalid DSR kind: {0}")]
    InvalidDsrKind(String),

    #[error("invalid DSR status: {0}")]
    InvalidDsrStatus(String),

    #[error("invalid requester: {0}")]
    InvalidRequester(String),

    #[error("invalid consent method: {0}")]
    InvalidConsentMethod(String),

    #[error("invalid signal source: {0}")]
    InvalidSignalSource(String),

    #[error("invalid sub-processor role: {0}")]
    InvalidSubProcessorRole(String),

    #[error("invalid transfer mechanism: {0}")]
    InvalidTransferMechanism(String),

    #[error("invalid obligation audience: {0}")]
    InvalidObligationAudience(String),

    #[error("invalid country code: {0}")]
    InvalidCountryCode(String),

    #[error("jurisdiction profile not found: {0}")]
    JurisdictionNotFound(String),

    #[error("subject not found: {0}")]
    SubjectNotFound(String),

    #[error("request not found: {0}")]
    RequestNotFound(String),

    #[error("request is not owned by the requester")]
    RequestNotOwned,

    #[error("request is not in an appealable state: {0}")]
    NotAppealable(String),

    #[error("request cannot be appealed twice")]
    AlreadyAppealed,

    #[error("appeal not found: {0}")]
    AppealNotFound(String),

    #[error("an appeal decision must be taken by a different operator than the original decision")]
    AppealSelfReview,

    #[error("verification required before this data may be disclosed")]
    VerificationRequired,

    #[error("verification challenge not found")]
    ChallengeNotFound,

    #[error("verification code expired")]
    ChallengeExpired,

    #[error("verification code mismatch")]
    ChallengeMismatch,

    #[error("verification attempts exhausted")]
    ChallengeExhausted,

    #[error("authorized-agent submission requires the agent's identity and authorization evidence")]
    AgentEvidenceMissing,

    #[error(
        "authorized-agent submission requires separate consumer verification before disclosure"
    )]
    AgentConsumerNotVerified,

    #[error("the subject is a known minor: {0}")]
    MinorSubject(String),

    #[error("sensitive data requires consent-first handling in this jurisdiction")]
    SensitiveRequiresConsent,

    #[error("legal hold blocks erasure: {0}")]
    LegalHoldActive(String),

    #[error("consent cannot be granted for the essential purpose")]
    EssentialNotConsentable,

    #[error(
        "the subject's encryption key has been destroyed (crypto-shredded); data is unrecoverable"
    )]
    KeyShredded,

    #[error("envelope encryption is not configured (missing KEK)")]
    EncryptionUnconfigured,

    #[error("encryption failure: {0}")]
    EncryptionFailure(String),

    #[error("export artifact not found: {0}")]
    ArtifactNotFound(String),

    #[error("export artifact expired")]
    ArtifactExpired,

    #[error("export artifact download limit reached")]
    ArtifactDownloadsExhausted,

    #[error("export passphrase mismatch")]
    ArtifactPassphraseMismatch,

    #[error("export packaging failed: {0}")]
    PackagingFailed(String),

    #[error("state conflict: {0}")]
    Conflict(String),

    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("upstream dependency failure: {0}")]
    Upstream(String),

    #[error("ID generation failure: {0}")]
    IdGeneration(String),

    #[error("storage failure: {0}")]
    Storage(String),
}

impl From<IdGenerationError> for PrivacyError {
    fn from(error: IdGenerationError) -> Self {
        Self::IdGeneration(error.to_string())
    }
}
