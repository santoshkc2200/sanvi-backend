//! `privacy` — GDPR / UK GDPR / APPI / US state privacy & compliance.
//!
//! The context answers, for any data subject: *what do we hold, why, under
//! what basis, for how long, and can you have it back or have it deleted?*
//! — as an automated, auditable workflow.
//!
//! Core machinery:
//! - **Jurisdiction profiles** are configuration (`privacy.jurisdiction_profiles`):
//!   consent model, sensitivity model, deadline windows, breach rules and the
//!   appeal/authority contacts. A new US state law is a data change plus a
//!   matrix-test row, never a release.
//! - **The directive engine** resolves one `PurposeDirective` per purpose
//!   (explicit action → guardian → universal opt-out signal → jurisdiction
//!   default → most protective). `Allowed` is the only value that permits
//!   processing. [`DirectiveResolver`] is the port every other context asks
//!   before processing anything non-essential; `Sec-GPC` signals are
//!   recorded before authentication and bind to the subject on the next
//!   authenticated request.
//! - **DSR saga**: every registered [`PersonalDataProvider`] (one per
//!   context — enforced by CI) participates in collect/erase/rectify with
//!   per-participant records, legal-hold checks, crypto-shred erasure and
//!   one-time, passphrase-encrypted, TTL-bound exports.
//! - **Governance**: Art. 30 records, risk assessments, the sub-processor
//!   registry (role + transfer mechanism), breach incidents with
//!   per-jurisdiction obligations, legal holds, retention sweep and the
//!   CCPA annual metrics disclosure.
//!
//! Nothing under `src/domain/` imports infrastructure crates — enforced by
//! `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::PrivacyConfig;
pub use domain::{
    AffectedSet, Appeal, AppealOutcome, AppealRepository, AuditWriter, AuthorizedAgent,
    AuthorizedAgentRepository, BreachIncident, BreachObligationCalculator, BreachRepository,
    BreachRuleSet, CONTEXT_NAMES, Clock, ConsentMethod, ConsentModel, ConsentRecord,
    ConsentRepository, ControllerRole, Correction, DATA_CLASSES, DataBundle, DataClass,
    DataClassDef, DataEntry, DataSubject, DataSubjectRequest, DirectiveInput, DirectiveRepository,
    DirectiveResolver, DirectiveSnapshot, DirectiveSource, DirectiveState, DsrId, DsrKind,
    DsrRepository, DsrStatus, EnvelopeCrypto, EraseMode, EraseReport, EventPublisher, EvidenceKind,
    ExportArtifactRepository, ExportPackager, Hasher, IdGenerator, JurisdictionEvidence,
    JurisdictionProfile, JurisdictionRef, JurisdictionRepository, LegalBasis, LegalHold,
    LegalHoldRepository, MOST_PROTECTIVE_CODE, NotificationObligation, Notifications,
    ObligationAudience, ParticipantRepository, ParticipantStatus, PersonalDataProvider,
    PreferenceSignal, PrivacyError, PrivacyEvent, ProcessingActivity, ProcessingActivityRepository,
    ProcessingPurpose, ProviderRegistry, PurposeDirective, RectifyPatch, Regime, RequestMetricRow,
    RequestMetricsRepository, Requester, RetainedItem, RetentionAction, RetentionReason,
    RetentionRule, RetentionRuleRepository, RiskAssessment, RiskAssessmentRepository,
    RiskAssessmentStatus, RiskAssessmentTrigger, SensitivityClass, SignalRepository, SignalSource,
    SubProcessor, SubProcessorNotifier, SubProcessorRepository, SubProcessorRole, SubjectId,
    SubjectIdentifier, SubjectKind, SubjectRef, SubjectRepository, TransferMechanism,
    VerificationChallenge, VerificationRepository, add_business_days, add_days,
    country_to_jurisdiction, most_protective, most_protective_profile, resolve, resolve_purpose,
    seeded_profile, seeded_profiles,
};

/// Wire privacy routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::{
        privacy_platform_routes, privacy_routes, privacy_scope, public_routes, tenant_scope_routes,
    };
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::catalog::{ROUTES as PERMISSIONED_ROUTES, Requirement};
}
