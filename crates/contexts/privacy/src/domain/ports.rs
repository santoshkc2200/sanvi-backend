//! The privacy context's ports. Two of them are the phase's key contracts:
//!
//! * [`PersonalDataProvider`] — implemented **by every context crate** (the
//!   CI test fails the build when a context registers no provider).
//! * [`DirectiveResolver`] — the question every other context asks before
//!   processing anything non-essential. It never returns "unknown": an
//!   unresolved purpose falls back to the jurisdiction default, and an
//!   unresolved jurisdiction falls back to the most protective profile.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{PlatformScope, ScopedTx};

use super::dsr::{
    Appeal, AppealId, AuthorizedAgent, AuthorizedAgentId, BreachIncident, BreachIncidentId,
    ConsentId, ConsentRecord, DataSubject, DataSubjectRequest, DsrId, ExportArtifact,
    ExportArtifactId, LegalHold, LegalHoldId, NotificationObligation, PreferenceSignal,
    PreferenceSignalId, ProcessingActivity, ProcessingActivityId, RetentionRule, RiskAssessment,
    RiskAssessmentId, SubProcessor, SubjectId, VerificationChallenge, VerificationChallengeId,
};
use super::error::PrivacyError;
use super::events::PrivacyEvent;
use super::jurisdiction::JurisdictionProfile;
use super::types::{
    DataBundle, DirectiveSnapshot, EraseMode, EraseReport, ProcessingPurpose, PurposeDirective,
    RectifyPatch, SubjectRef,
};

// ---------------------------------------------------------------------------
// The phase's key contract, implemented by every context.
// ---------------------------------------------------------------------------

/// Implemented by every context crate: what personal data the context holds
/// for a subject, and how to hand it back, fix it or destroy it. The CI
/// provider-registry test enumerates the workspace's context crates and
/// fails the build when any context registers no provider.
#[async_trait]
pub trait PersonalDataProvider: Send + Sync {
    /// The context this provider answers for (`tenancy`, `identity`, …).
    fn context_name(&self) -> &'static str;

    /// The data classes this context holds (the declarative data map).
    fn data_classes(&self) -> Vec<super::data_map::DataClassDef>;

    /// Collect everything the context holds for the subject.
    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError>;

    /// Erase/anonymise the subject's data. The report must state what was
    /// retained and why; a provider that silently succeeds is a lie.
    async fn erase(
        &self,
        subject: &SubjectRef,
        mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError>;

    /// Fields this provider can actually correct through this workflow.
    fn rectifiable_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Apply rectification corrections owned by this context.
    async fn rectify(&self, subject: &SubjectRef, patch: &RectifyPatch)
    -> Result<(), PrivacyError>;
}

/// The provider registry: the composition root registers every context's
/// provider here; the DSR saga fans out over it and the CI test proves it is
/// complete.
pub type ProviderRegistry = Vec<Arc<dyn PersonalDataProvider>>;

/// Find a provider by context name.
pub fn provider<'a>(
    registry: &'a ProviderRegistry,
    context: &str,
) -> Option<&'a Arc<dyn PersonalDataProvider>> {
    registry.iter().find(|p| p.context_name() == context)
}

// ---------------------------------------------------------------------------
// The second key contract: directive resolution.
// ---------------------------------------------------------------------------

#[async_trait]
pub trait DirectiveResolver: Send + Sync {
    /// Never returns "unknown": an unresolved purpose falls back to the
    /// jurisdiction default, and an unresolved jurisdiction falls back to
    /// the most protective profile.
    async fn resolve(
        &self,
        subject: &SubjectRef,
        purpose: ProcessingPurpose,
    ) -> Result<PurposeDirective, PrivacyError>;

    /// The full resolved state (what the privacy centre renders).
    async fn snapshot(&self, subject: &SubjectRef) -> Result<DirectiveSnapshot, PrivacyError>;
}

// ---------------------------------------------------------------------------
// Persistence ports.
// ---------------------------------------------------------------------------

#[async_trait]
pub trait JurisdictionRepository: Send + Sync {
    async fn resolve_evidence(
        &self,
        scope: &mut PlatformScope<'_>,
        evidence: &[(super::types::EvidenceKind, String)],
    ) -> Result<
        (
            super::types::JurisdictionRef,
            Vec<super::types::JurisdictionEvidence>,
        ),
        PrivacyError,
    >;
    async fn set_mappings(
        &self,
        scope: &mut PlatformScope<'_>,
        code: &str,
        inputs: &[String],
    ) -> Result<(), PrivacyError>;

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        code: &str,
    ) -> Result<Option<JurisdictionProfile>, PrivacyError>;
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<JurisdictionProfile>, PrivacyError>;
    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        profile: &JurisdictionProfile,
        updated_by: UserId,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait SubjectRepository: Send + Sync {
    /// A verified restriction/objection survives consent changes until explicitly released.
    async fn processing_restricted(
        &self,
        scope: &mut PlatformScope<'_>,
        key: &str,
    ) -> Result<bool, PrivacyError>;

    async fn find_by_key(
        &self,
        scope: &mut PlatformScope<'_>,
        key: &str,
    ) -> Result<Option<DataSubject>, PrivacyError>;
    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: SubjectId,
    ) -> Result<Option<DataSubject>, PrivacyError>;
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        subject: &DataSubject,
    ) -> Result<(), PrivacyError>;
    async fn set_minor(
        &self,
        scope: &mut PlatformScope<'_>,
        id: SubjectId,
        is_minor: bool,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait DsrRepository: Send + Sync {
    async fn record_fulfillment(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        summary: &str,
        evidence: &str,
    ) -> Result<(), PrivacyError>;
    async fn fulfillment_summary(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
    ) -> Result<Option<String>, PrivacyError>;

    async fn release_restriction(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
    ) -> Result<(), PrivacyError>;

    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        request: &DataSubjectRequest,
    ) -> Result<(), PrivacyError>;
    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
    ) -> Result<Option<DataSubjectRequest>, PrivacyError>;
    async fn find_by_id_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        id: DsrId,
    ) -> Result<Option<DataSubjectRequest>, PrivacyError>;
    /// Requests awaiting verification, for the subject's own lookup.
    async fn find_by_subject_unverified(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<Vec<DataSubjectRequest>, PrivacyError>;
    async fn open_requests(
        &self,
        scope: &mut PlatformScope<'_>,
        statuses: &[super::types::DsrStatus],
        kind: Option<super::types::DsrKind>,
        limit: u32,
    ) -> Result<Vec<DataSubjectRequest>, PrivacyError>;
    async fn list_operator(
        &self,
        scope: &mut PlatformScope<'_>,
        status: Option<super::types::DsrStatus>,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<DataSubjectRequest>, PrivacyError>;
    async fn list_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        status: Option<super::types::DsrStatus>,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<DataSubjectRequest>, PrivacyError>;
    async fn set_status(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        status: super::types::DsrStatus,
        completed_at: Option<DateTime<Utc>>,
        rejection_reason: Option<String>,
    ) -> Result<(), PrivacyError>;
    async fn set_rejected_by(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        actor: UserId,
    ) -> Result<(), PrivacyError>;
    async fn extend(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        extended_to: DateTime<Utc>,
    ) -> Result<(), PrivacyError>;
    async fn acknowledge(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        at: DateTime<Utc>,
    ) -> Result<(), PrivacyError>;
    async fn verify(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        at: DateTime<Utc>,
    ) -> Result<(), PrivacyError>;
    async fn link_artifact(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        artifact_id: ExportArtifactId,
    ) -> Result<(), PrivacyError>;
    /// Erasure requests whose subject is inside `tenant_id` — the
    /// impersonation privacy lock.
    async fn has_open_erasure_for_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<bool, PrivacyError>;
    /// Counters for the annual metrics job.
    async fn metrics_by_kind(
        &self,
        scope: &mut PlatformScope<'_>,
        year: i32,
    ) -> Result<Vec<RequestMetricRow>, PrivacyError>;
}

#[derive(Debug, Clone)]
pub struct RequestMetricRow {
    pub jurisdiction: String,
    pub kind: String,
    pub received: i64,
    pub complied: i64,
    pub denied: i64,
    pub median_days: Option<f64>,
}

#[async_trait]
pub trait AppealRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        appeal: &Appeal,
    ) -> Result<(), PrivacyError>;
    async fn find_by_request(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<Option<Appeal>, PrivacyError>;
    async fn find_open(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<Appeal>, PrivacyError>;
    async fn decide(
        &self,
        scope: &mut PlatformScope<'_>,
        appeal: &Appeal,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait ParticipantRepository: Send + Sync {
    /// Initialise one participant row per registered context (idempotent).
    async fn ensure_participants(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
        contexts: &[&'static str],
    ) -> Result<(), PrivacyError>;
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<BTreeMap<String, ParticipantRow>, PrivacyError>;
    async fn set_status(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
        context: &str,
        status: ParticipantStatus,
        report: Option<serde_json::Value>,
        error: Option<String>,
    ) -> Result<(), PrivacyError>;
    async fn record_attempt(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
        context: &str,
    ) -> Result<(), PrivacyError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipantStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Retained,
}

impl ParticipantStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Retained => "retained",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "retained" => Ok(Self::Retained),
            other => Err(PrivacyError::InvalidDsrStatus(format!(
                "invalid participant status: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParticipantRow {
    pub context: String,
    pub status: ParticipantStatus,
    pub report: Option<serde_json::Value>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[async_trait]
pub trait ConsentRepository: Send + Sync {
    async fn append(
        &self,
        scope: &mut PlatformScope<'_>,
        record: &ConsentRecord,
    ) -> Result<(), PrivacyError>;
    async fn latest_for_purpose(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: Option<SubjectId>,
        subject_key: &str,
        purpose: ProcessingPurpose,
    ) -> Result<Option<ConsentRecord>, PrivacyError>;
    async fn list_for_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: Option<SubjectId>,
        subject_key: &str,
    ) -> Result<Vec<ConsentRecord>, PrivacyError>;
    /// Ledger rows whose opt-out has not yet propagated to downstream
    /// processors.
    async fn unpropagated_opt_outs(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<ConsentRecord>, PrivacyError>;
    async fn mark_propagated(
        &self,
        scope: &mut PlatformScope<'_>,
        record_id: ConsentId,
    ) -> Result<(), PrivacyError>;
    /// Rows past their retention period (the sweep deletes/anonymises them).
    async fn expired(
        &self,
        scope: &mut PlatformScope<'_>,
        older_than: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ConsentRecord>, PrivacyError>;
    async fn delete(
        &self,
        scope: &mut PlatformScope<'_>,
        record_id: ConsentId,
    ) -> Result<(), PrivacyError>;
    /// Pseudonymise the subject's ledger rows (proof survives, identifiers
    /// are replaced). Returns the number of rows anonymised.
    async fn anonymise_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_key: &str,
        anonymised: &SubjectRef,
    ) -> Result<usize, PrivacyError>;
    /// Pseudonymise exactly one expired ledger row, leaving newer rows for
    /// the same subject untouched.
    async fn anonymise_record(
        &self,
        scope: &mut PlatformScope<'_>,
        record_id: ConsentId,
        anonymised: &SubjectRef,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait SignalRepository: Send + Sync {
    async fn append(
        &self,
        scope: &mut PlatformScope<'_>,
        signal: &PreferenceSignal,
    ) -> Result<(), PrivacyError>;
    async fn latest_for_device(
        &self,
        scope: &mut PlatformScope<'_>,
        device_ref: &str,
    ) -> Result<Option<PreferenceSignal>, PrivacyError>;
    /// Unbound signals for a subject's identifier values (bind on the next
    /// authenticated request — binding never unbinds).
    async fn unbound_for_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject: &SubjectRef,
    ) -> Result<Vec<PreferenceSignal>, PrivacyError>;
    async fn bind(
        &self,
        scope: &mut PlatformScope<'_>,
        signal_id: PreferenceSignalId,
        subject_id: SubjectId,
        jurisdiction: &str,
    ) -> Result<(), PrivacyError>;

    /// Bind every unbound signal observed on this device to the subject
    /// (the GPC middleware's binding path: the device cookie is the link
    /// between the pre-auth signal and the identified subject).
    async fn bind_by_device(
        &self,
        scope: &mut PlatformScope<'_>,
        device_ref: &str,
        subject_id: SubjectId,
        jurisdiction: &str,
    ) -> Result<usize, PrivacyError>;

    /// Signals bound to a subject (the directive engine reads these for
    /// authenticated subjects: binding never unbinds).
    async fn list_bound_for_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<Vec<PreferenceSignal>, PrivacyError>;
    async fn expired(
        &self,
        scope: &mut PlatformScope<'_>,
        older_than: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<PreferenceSignal>, PrivacyError>;
    async fn delete(
        &self,
        scope: &mut PlatformScope<'_>,
        signal_id: PreferenceSignalId,
    ) -> Result<(), PrivacyError>;
    /// Delete the subject's unbound signals (device observations die with
    /// erasure; bound rows were already anonymised via their subject).
    async fn delete_unbound(
        &self,
        scope: &mut PlatformScope<'_>,
        subject: &SubjectRef,
    ) -> Result<usize, PrivacyError>;
}

#[async_trait]
pub trait DirectiveRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        directive: &PurposeDirective,
        subject: &SubjectRef,
    ) -> Result<(), PrivacyError>;
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_key: &str,
        purpose: ProcessingPurpose,
    ) -> Result<Option<PurposeDirective>, PrivacyError>;
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_key: &str,
    ) -> Result<Vec<PurposeDirective>, PrivacyError>;
    /// Supersede every directive of a subject (used by erasure).
    async fn supersede_all(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_key: &str,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait RetentionRuleRepository: Send + Sync {
    async fn list(&self, scope: &mut PlatformScope<'_>)
    -> Result<Vec<RetentionRule>, PrivacyError>;
    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        rule: &RetentionRule,
        updated_by: UserId,
    ) -> Result<(), PrivacyError>;
    async fn ensure_seeded(&self, scope: &mut PlatformScope<'_>) -> Result<usize, PrivacyError>;
}

#[async_trait]
pub trait LegalHoldRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        hold: &LegalHold,
    ) -> Result<(), PrivacyError>;
    async fn active_for_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject: &SubjectRef,
    ) -> Result<Vec<LegalHold>, PrivacyError>;
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        include_released: bool,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<LegalHold>, PrivacyError>;
    async fn release(
        &self,
        scope: &mut PlatformScope<'_>,
        id: LegalHoldId,
        released_by: UserId,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait SubProcessorRepository: Send + Sync {
    async fn list_public(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<SubProcessor>, PrivacyError>;
    async fn list_all(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<SubProcessor>, PrivacyError>;
    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        processor: &SubProcessor,
    ) -> Result<(), PrivacyError>;
    async fn remove(&self, scope: &mut PlatformScope<'_>, name: &str) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait BreachRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        incident: &BreachIncident,
    ) -> Result<(), PrivacyError>;
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: BreachIncidentId,
    ) -> Result<Option<BreachIncident>, PrivacyError>;
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<BreachIncident>, PrivacyError>;
    async fn save_obligation(
        &self,
        scope: &mut PlatformScope<'_>,
        incident_id: BreachIncidentId,
        obligation: &NotificationObligation,
    ) -> Result<(), PrivacyError>;
    async fn list_obligations(
        &self,
        scope: &mut PlatformScope<'_>,
        incident_id: BreachIncidentId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<NotificationObligation>, PrivacyError>;
    async fn mark_notified(
        &self,
        scope: &mut PlatformScope<'_>,
        incident_id: BreachIncidentId,
        jurisdiction: &str,
        audience: super::dsr::ObligationAudience,
        artifact: Option<serde_json::Value>,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait SubjectKeyRepository: Send + Sync {
    /// Store the wrapped DEK for a subject.
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
        wrapped_dek: Vec<u8>,
        kek_id: &str,
    ) -> Result<(), PrivacyError>;
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<Option<SubjectKeyRow>, PrivacyError>;
    /// Crypto-shred: destroy the key. After this, previously readable
    /// ciphertext is unrecoverable and decrypt returns `KeyShredded`.
    async fn destroy(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<(), PrivacyError>;
    async fn exists(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<bool, PrivacyError>;
}

#[derive(Debug, Clone)]
pub struct SubjectKeyRow {
    pub subject_id: SubjectId,
    pub wrapped_dek: Vec<u8>,
    pub kek_id: String,
    pub created_at: DateTime<Utc>,
    pub destroyed_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait ExportArtifactRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        artifact: &ExportArtifact,
    ) -> Result<(), PrivacyError>;
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ExportArtifactId,
    ) -> Result<Option<ExportArtifact>, PrivacyError>;
    async fn find_by_request(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<Option<ExportArtifact>, PrivacyError>;
    async fn record_download(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ExportArtifactId,
    ) -> Result<(), PrivacyError>;
    async fn expired(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<ExportArtifact>, PrivacyError>;
    async fn delete(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ExportArtifactId,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait VerificationRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        challenge: &VerificationChallenge,
    ) -> Result<(), PrivacyError>;
    async fn find_by_request(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<Option<VerificationChallenge>, PrivacyError>;
    async fn record_attempt(
        &self,
        scope: &mut PlatformScope<'_>,
        id: VerificationChallengeId,
    ) -> Result<(), PrivacyError>;
    async fn consume(
        &self,
        scope: &mut PlatformScope<'_>,
        id: VerificationChallengeId,
    ) -> Result<(), PrivacyError>;
}

#[async_trait]
pub trait RequestMetricsRepository: Send + Sync {
    async fn upsert_yearly(
        &self,
        scope: &mut PlatformScope<'_>,
        year: i32,
        rows: &[RequestMetricRow],
    ) -> Result<(), PrivacyError>;
    async fn list_yearly(
        &self,
        scope: &mut PlatformScope<'_>,
        year: i32,
    ) -> Result<Vec<YearlyMetricRow>, PrivacyError>;
}

/// Art. 30 processing records (versioned; `active` marks the current row).
#[async_trait]
pub trait ProcessingActivityRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        activity: &ProcessingActivity,
    ) -> Result<(), PrivacyError>;
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ProcessingActivityId,
    ) -> Result<Option<ProcessingActivity>, PrivacyError>;
    async fn list_active(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<ProcessingActivity>, PrivacyError>;
}

/// DPIA + US state data-protection assessments.
#[async_trait]
pub trait RiskAssessmentRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        assessment: &RiskAssessment,
    ) -> Result<(), PrivacyError>;
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: RiskAssessmentId,
    ) -> Result<Option<RiskAssessment>, PrivacyError>;
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<RiskAssessment>, PrivacyError>;
}

/// Authorized-agent submissions (CCPA).
#[async_trait]
pub trait AuthorizedAgentRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        agent: &AuthorizedAgent,
    ) -> Result<(), PrivacyError>;
    async fn find_by_request(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<Option<AuthorizedAgent>, PrivacyError>;
    async fn mark_consumer_verified(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
        verified_by: UserId,
    ) -> Result<(), PrivacyError>;
}

#[derive(Debug, Clone)]
pub struct YearlyMetricRow {
    pub year: i32,
    pub jurisdiction: String,
    pub kind: String,
    pub received: i32,
    pub complied: i32,
    pub denied: i32,
    pub median_days: Option<f64>,
}

// ---------------------------------------------------------------------------
// Supporting ports.
// ---------------------------------------------------------------------------

/// Hashes and verifies verification tokens/codes/OTPs. The adapter holds no
/// plaintext; challenges store only hashes.
pub trait Hasher: Send + Sync {
    fn hash(&self, value: &str) -> String;
    fn verify(&self, value: &str, hash: &str) -> bool;
}

/// Encrypts/decrypts export artifacts and arbitrary subject payloads
/// (envelope encryption: per-subject DEK wrapped by a KEK).
#[async_trait]
pub trait EnvelopeCrypto: Send + Sync {
    /// Create (or return the existing) wrapped DEK for the subject.
    async fn create_subject_key(&self, subject_id: SubjectId) -> Result<Vec<u8>, PrivacyError>;
    /// Wrap a DEK with the KEK.
    fn wrap(&self, dek: &[u8], kek_id: &str) -> Result<Vec<u8>, PrivacyError>;
    /// Unwrap a DEK with the KEK.
    fn unwrap(&self, wrapped: &[u8], kek_id: &str) -> Result<Vec<u8>, PrivacyError>;
    /// Encrypt with the subject's DEK (returns ciphertext with the nonce).
    async fn encrypt_for_subject(
        &self,
        subject_id: SubjectId,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, PrivacyError>;
    /// Decrypt with the subject's DEK. After the key was destroyed this
    /// returns `KeyShredded` — the typed, non-leaking answer.
    async fn decrypt_for_subject(
        &self,
        subject_id: SubjectId,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, PrivacyError>;
    /// Destroy the subject's DEK (crypto-shred).
    async fn destroy_subject_key(&self, subject_id: SubjectId) -> Result<(), PrivacyError>;
    /// Encrypt an export archive with a passphrase (AES-256-GCM + PBKDF2).
    fn encrypt_with_passphrase(
        &self,
        plaintext: &[u8],
        passphrase: &str,
    ) -> Result<Vec<u8>, PrivacyError>;
    fn decrypt_with_passphrase(
        &self,
        ciphertext: &[u8],
        passphrase: &str,
    ) -> Result<Vec<u8>, PrivacyError>;
}

/// Packages provider bundles into the deliverable archive: machine-readable
/// JSON + human-readable HTML/CSV + a manifest with per-file checksums.
pub trait ExportPackager: Send + Sync {
    fn package(
        &self,
        subject: &SubjectRef,
        bundles: Vec<DataBundle>,
    ) -> Result<Vec<u8>, PrivacyError>;
}

/// Sends verification codes and complete export credentials out of band.
///
/// Both payloads are live credentials, so neither may be written to
/// `platform.outbox` and relayed through Redis the way every other privacy
/// notification is. They stay direct calls, made *after* the transaction that
/// minted them has committed.
///
/// `request_id` is what the adapter builds a delivery-idempotency key from, so
/// a transport retry replays rather than mailing a second code.
/// `send_export_credentials` takes the recipient explicitly because a
/// [`SubjectRef`] does not always carry an email identifier — resolving it is
/// the caller's decision, not something the adapter may guess.
#[async_trait]
pub trait Notifications: Send + Sync {
    async fn send_verification_code(
        &self,
        to: &str,
        code: &str,
        request_id: DsrId,
    ) -> Result<(), PrivacyError>;
    async fn send_export_credentials(
        &self,
        subject: &SubjectRef,
        recipient: &str,
        token: &str,
        passphrase: &str,
        request_id: DsrId,
    ) -> Result<(), PrivacyError>;
}

/// Propagates honoured opt-outs to downstream processors (ad platforms,
/// mail providers) within the profile's business-day window.
#[async_trait]
pub trait SubProcessorNotifier: Send + Sync {
    async fn propagate_opt_out(
        &self,
        record: &ConsentRecord,
        processors: &[SubProcessor],
    ) -> Result<(), PrivacyError>;
    async fn notify_removal(
        &self,
        processor: &SubProcessor,
        tenant_admins: Vec<String>,
    ) -> Result<(), PrivacyError>;
}

/// One destination an erasure must reach beyond the provider registry:
/// the archives, the backup window, the caches, the search indexes, the
/// ad-platform suppression lists. Adapters live with the facility they
/// drive (the composition root adapts foreign contexts onto this port);
/// [`crate::application::erasure_propagation::PropagateErasure`] fans out
/// over the registered destinations and every outcome is ledgered.
#[async_trait]
pub trait ErasureDestination: Send + Sync {
    /// Stable ledger name (`archives`, `caches`, `backup_window`, …).
    fn name(&self) -> &'static str;

    /// What reaching this destination means, in one sentence — the ledger
    /// records it verbatim so the evidence pack needs no second source.
    fn description(&self) -> String;

    async fn propagate_erasure(
        &self,
        subject: &SubjectRef,
        tenant_id: Option<TenantId>,
    ) -> Result<EraseDestinationOutcome, PrivacyError>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EraseDestinationOutcome {
    /// Rows/entries purged at this destination (0 when documented-only).
    pub purged: u64,
    /// What this destination lawfully retains: each item with its basis
    /// and, where the retention is time-boxed, the date it ends.
    pub retained: Vec<DestinationRetainedItem>,
}

impl EraseDestinationOutcome {
    pub fn purged(purged: u64) -> Self {
        Self {
            purged,
            retained: Vec::new(),
        }
    }

    pub fn retained(retained: Vec<DestinationRetainedItem>) -> Self {
        Self {
            purged: 0,
            retained,
        }
    }

    pub fn status(&self) -> &'static str {
        if self.retained.is_empty() {
            "purged"
        } else {
            "retained"
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DestinationRetainedItem {
    /// What is retained (class/rows and where they live).
    pub detail: String,
    /// Why (the lawful basis, in plain words).
    pub reason: String,
    /// When the retention ends, for time-boxed retention.
    pub retains_until: Option<DateTime<Utc>>,
}

/// Computes per-jurisdiction breach notification obligations.
pub trait BreachObligationCalculator: Send + Sync {
    fn compute(
        &self,
        incident: &BreachIncident,
        profiles: &[JurisdictionProfile],
        now: DateTime<Utc>,
    ) -> Vec<NotificationObligation>;
}

#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        events: &[PrivacyEvent],
    ) -> Result<(), PrivacyError>;
}

/// Writes privacy actions into the phase-03 audit chain (defined here so the
/// privacy context never imports the audit context; the composition root
/// adapts the audit context's writer).
#[async_trait]
pub trait AuditWriter: Send + Sync {
    async fn record(
        &self,
        scope: &mut PlatformScope<'_>,
        actor: Option<UserId>,
        action: String,
        resource: String,
        detail: serde_json::Value,
    ) -> Result<(), PrivacyError>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub trait IdGenerator: Send + Sync {
    fn next_subject_id(&self) -> Result<SubjectId, PrivacyError>;
    fn next_dsr_id(&self) -> Result<DsrId, PrivacyError>;
    fn next_appeal_id(&self) -> Result<AppealId, PrivacyError>;
    fn next_authorized_agent_id(&self) -> Result<AuthorizedAgentId, PrivacyError>;
    fn next_breach_incident_id(&self) -> Result<BreachIncidentId, PrivacyError>;
    fn next_consent_id(&self) -> Result<ConsentId, PrivacyError>;
    fn next_export_artifact_id(&self) -> Result<ExportArtifactId, PrivacyError>;
    fn next_legal_hold_id(&self) -> Result<LegalHoldId, PrivacyError>;
    fn next_preference_signal_id(&self) -> Result<PreferenceSignalId, PrivacyError>;
    fn next_processing_activity_id(&self) -> Result<ProcessingActivityId, PrivacyError>;
    fn next_risk_assessment_id(&self) -> Result<RiskAssessmentId, PrivacyError>;
    fn next_verification_challenge_id(&self) -> Result<VerificationChallengeId, PrivacyError>;
}

/// The registry of every context that must register a provider. The CI test
/// derives the workspace's context crates and asserts this list matches
/// exactly; the DSR saga asserts every registered provider participated.
pub const CONTEXT_NAMES: [&str; 17] = [
    "system",
    "tenancy",
    "identity",
    "access",
    "audit",
    "platform_admin",
    "billing",
    "accounting",
    "privacy",
    "localization",
    "theming",
    "domains",
    "payments",
    "advertising",
    "media",
    "notifications",
    "product_access",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_names_are_unique_and_sorted() {
        let mut sorted = CONTEXT_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), CONTEXT_NAMES.len(), "duplicate context names");
    }

    #[test]
    fn participant_status_roundtrips() {
        for status in [
            ParticipantStatus::Pending,
            ParticipantStatus::InProgress,
            ParticipantStatus::Completed,
            ParticipantStatus::Failed,
            ParticipantStatus::Retained,
        ] {
            assert_eq!(ParticipantStatus::parse(status.as_str()).unwrap(), status);
        }
    }
}
