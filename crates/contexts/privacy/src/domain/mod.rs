//! The privacy context's domain layer: value types, jurisdiction profiles,
//! the directive engine, the DSR aggregate and the ports. Pure — no
//! infrastructure imports (enforced by `scripts/check-layering.sh`).

pub mod data_map;
pub mod directive;
pub mod dsr;
pub mod error;
pub mod events;
pub mod jurisdiction;
pub mod ports;
pub mod types;

pub use data_map::{DATA_CLASSES, DataClassDef};
pub use directive::{DirectiveInput, jurisdiction_default, resolve_purpose, resolve_snapshot};
pub use dsr::{
    AffectedSet, Appeal, AppealId, AppealOutcome, AuthorizedAgent, AuthorizedAgentId,
    BreachIncident, BreachIncidentId, ConsentId, ConsentRecord, ControllerRole, DataSubject,
    DataSubjectRequest, DsrId, ExportArtifact, ExportArtifactId, LegalHold, LegalHoldId,
    NotificationObligation, ObligationAudience, PreferenceSignal, PreferenceSignalId,
    ProcessingActivity, ProcessingActivityId, RetentionRule, RiskAssessment, RiskAssessmentId,
    RiskAssessmentStatus, RiskAssessmentTrigger, SubProcessor, SubProcessorRole, SubjectId,
    TransferMechanism, VerificationChallenge, VerificationChallengeId,
};
pub use error::PrivacyError;
pub use events::PrivacyEvent;
pub use jurisdiction::{
    AuthorityContact, BreachRuleSet, JurisdictionProfile, MOST_PROTECTIVE_CODE, RESOLUTION_ORDER,
    add_business_days, add_days, country_to_jurisdiction, global_operating_profile,
    most_protective, most_protective_profile, resolve, seeded_profile, seeded_profiles,
};
pub use ports::{
    AppealRepository, AuditWriter, AuthorizedAgentRepository, BreachObligationCalculator,
    BreachRepository, CONTEXT_NAMES, Clock, ConsentRepository, DestinationRetainedItem,
    DirectiveRepository, DirectiveResolver, DsrRepository, EnvelopeCrypto, EraseDestinationOutcome,
    ErasureDestination, EventPublisher, ExportArtifactRepository, ExportPackager, Hasher,
    IdGenerator, JurisdictionRepository, LegalHoldRepository, Notifications, ParticipantRepository,
    ParticipantRow, ParticipantStatus, PersonalDataProvider, ProcessingActivityRepository,
    ProviderRegistry, RequestMetricRow, RequestMetricsRepository, RetentionRuleRepository,
    RiskAssessmentRepository, SignalRepository, SubProcessorNotifier, SubProcessorRepository,
    SubjectKeyRepository, SubjectKeyRow, SubjectRepository, VerificationRepository,
    YearlyMetricRow, provider,
};
pub use types::{
    ConsentMethod, ConsentModel, Correction, DataBundle, DataClass, DataEntry, DirectiveSnapshot,
    DirectiveSource, DirectiveState, DsrKind, DsrStatus, EraseMode, EraseReport, EvidenceKind,
    JurisdictionEvidence, JurisdictionRef, LegalBasis, ProcessingPurpose, PurposeDirective,
    RectifyPatch, Regime, Requester, RetainedItem, RetentionAction, RetentionReason,
    SensitivityClass, SignalSource, SubjectIdentifier, SubjectKind, SubjectRef,
};
