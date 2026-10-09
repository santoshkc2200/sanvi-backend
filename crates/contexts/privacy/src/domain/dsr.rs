//! The data subject request aggregate: kinds, status machine and the
//! per-jurisdiction clocks (acknowledgement, response, extension, opt-out
//! effectiveness, appeal) computed from the subject's jurisdiction profile.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::PrivacyError;
use super::jurisdiction::{JurisdictionProfile, add_business_days, add_days};
use super::types::{DsrKind, DsrStatus, JurisdictionRef, Requester, SubjectRef};

pub type DsrId = sanvi_kernel::Id<DsrTag>;
pub type SubjectId = sanvi_kernel::Id<SubjectTag>;
pub type AppealId = sanvi_kernel::Id<AppealTag>;
pub type AuthorizedAgentId = sanvi_kernel::Id<AuthorizedAgentTag>;
pub type BreachIncidentId = sanvi_kernel::Id<BreachIncidentTag>;
pub type ConsentId = sanvi_kernel::Id<ConsentTag>;
pub type ExportArtifactId = sanvi_kernel::Id<ExportArtifactTag>;
pub type LegalHoldId = sanvi_kernel::Id<LegalHoldTag>;
pub type PreferenceSignalId = sanvi_kernel::Id<PreferenceSignalTag>;
pub type ProcessingActivityId = sanvi_kernel::Id<ProcessingActivityTag>;
pub type RiskAssessmentId = sanvi_kernel::Id<RiskAssessmentTag>;
pub type VerificationChallengeId = sanvi_kernel::Id<VerificationChallengeTag>;

#[derive(Debug, Clone, Copy)]
pub enum DsrTag {}

#[derive(Debug, Clone, Copy)]
pub enum SubjectTag {}

#[derive(Debug, Clone, Copy)]
pub enum AppealTag {}

#[derive(Debug, Clone, Copy)]
pub enum AuthorizedAgentTag {}

#[derive(Debug, Clone, Copy)]
pub enum BreachIncidentTag {}

#[derive(Debug, Clone, Copy)]
pub enum ConsentTag {}

#[derive(Debug, Clone, Copy)]
pub enum ExportArtifactTag {}

#[derive(Debug, Clone, Copy)]
pub enum LegalHoldTag {}

#[derive(Debug, Clone, Copy)]
pub enum PreferenceSignalTag {}

#[derive(Debug, Clone, Copy)]
pub enum ProcessingActivityTag {}

#[derive(Debug, Clone, Copy)]
pub enum RiskAssessmentTag {}

#[derive(Debug, Clone, Copy)]
pub enum VerificationChallengeTag {}

/// The persisted subject: identity anchor + resolved jurisdiction.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DataSubject {
    #[schema(value_type = String)]
    pub id: SubjectId,
    #[schema(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
    pub subject_ref: SubjectRef,
    pub jurisdiction: JurisdictionRef,
    pub jurisdiction_evidence: Vec<super::types::JurisdictionEvidence>,
    pub is_minor: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The request aggregate; every clock is set at submission from the
/// jurisdiction profile and recorded, not recomputed later.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DataSubjectRequest {
    #[schema(value_type = String)]
    pub id: DsrId,
    #[schema(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
    #[schema(value_type = String)]
    pub subject_id: SubjectId,
    pub subject_ref: SubjectRef,
    pub kind: DsrKind,
    pub status: DsrStatus,
    pub received_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub verified_at: Option<DateTime<Utc>>,
    pub due_at: DateTime<Utc>,
    pub extended_to: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub jurisdiction: JurisdictionRef,
    pub submitted_by: Requester,
    #[schema(value_type = Option<String>)]
    pub agent_id: Option<AuthorizedAgentId>,
    #[schema(value_type = Option<String>)]
    pub requester_ip: Option<IpAddr>,
    pub requester_note: Option<String>,
    pub rejection_reason: Option<String>,
    #[schema(value_type = Option<String>)]
    pub rejected_by: Option<UserId>,
    #[schema(value_type = Option<String>)]
    pub appeal_id: Option<AppealId>,
    #[schema(value_type = Option<String>)]
    pub export_artifact_id: Option<ExportArtifactId>,
}

impl DataSubjectRequest {
    /// The clock this request runs against: the earliest un-met deadline.
    pub fn effective_due(&self) -> DateTime<Utc> {
        self.extended_to.unwrap_or(self.due_at)
    }

    /// The acknowledgment deadline (US profiles): response receipt must be
    /// recorded within this many business days.
    pub fn ack_due(&self, profile: &JurisdictionProfile) -> Option<DateTime<Utc>> {
        profile
            .ack_days
            .map(|days| add_business_days(self.received_at, days))
    }

    /// How long until this request breaches the internal SLA (in days, can
    /// be negative when overdue).
    pub fn days_until_due(&self, now: DateTime<Utc>) -> f64 {
        let due = self.effective_due();
        (due - now).num_seconds() as f64 / 86_400.0
    }

    /// Whether the request may transition to `in_progress` (verified, not
    /// terminal, not held).
    pub fn can_start(&self) -> bool {
        matches!(
            self.status,
            DsrStatus::AwaitingVerification | DsrStatus::InProgress
        ) && self.verified_at.is_some()
    }

    pub fn mark_acknowledged(&mut self, now: DateTime<Utc>) -> Result<(), PrivacyError> {
        if self.acknowledged_at.is_some() {
            return Ok(());
        }
        self.acknowledged_at = Some(now);
        Ok(())
    }

    pub fn mark_verified(&mut self, now: DateTime<Utc>) -> Result<(), PrivacyError> {
        if self.verified_at.is_some() {
            return Ok(());
        }
        self.verified_at = Some(now);
        Ok(())
    }

    pub fn complete(&mut self, now: DateTime<Utc>) -> Result<(), PrivacyError> {
        if self.status.is_terminal() {
            return Ok(());
        }
        self.status = DsrStatus::Completed;
        self.completed_at = Some(now);
        Ok(())
    }

    pub fn reject(&mut self, reason: String, now: DateTime<Utc>) -> Result<(), PrivacyError> {
        if self.status.is_terminal() {
            return Err(PrivacyError::Conflict(format!(
                "request is already {}",
                self.status.as_str()
            )));
        }
        self.status = DsrStatus::Rejected;
        self.completed_at = Some(now);
        self.rejection_reason = Some(reason);
        Ok(())
    }

    pub fn extend(
        &mut self,
        now: DateTime<Utc>,
        profile: &JurisdictionProfile,
    ) -> Result<(), PrivacyError> {
        if self.status.is_terminal() {
            return Err(PrivacyError::Conflict(
                "request is already closed".to_string(),
            ));
        }
        if self.extended_to.is_some() {
            return Err(PrivacyError::Conflict(
                "deadline already extended once".to_string(),
            ));
        }
        if profile.extension_days == 0 {
            return Err(PrivacyError::Conflict(
                "this jurisdiction does not allow a deadline extension".to_string(),
            ));
        }
        let base = self.due_at;
        self.extended_to = Some(add_days(base.max(now), profile.extension_days));
        Ok(())
    }
}

/// The appeal aggregate (US states with an appeal right).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Appeal {
    #[schema(value_type = String)]
    pub id: AppealId,
    #[schema(value_type = String)]
    pub request_id: DsrId,
    pub reason: String,
    pub received_at: DateTime<Utc>,
    pub due_at: DateTime<Utc>,
    pub outcome: Option<AppealOutcome>,
    pub decided_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub decided_by: Option<UserId>,
    pub decision_reason: Option<String>,
    pub authority_notice: Option<super::jurisdiction::AuthorityContact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppealOutcome {
    Upheld,
    Denied,
}

impl Appeal {
    pub fn decide(
        &mut self,
        outcome: AppealOutcome,
        decided_by: UserId,
        reason: String,
        now: DateTime<Utc>,
        rejected_by: Option<UserId>,
    ) -> Result<(), PrivacyError> {
        if self.decided_at.is_some() {
            return Err(PrivacyError::Conflict("appeal already decided".to_string()));
        }
        // Second-operator review: the person deciding must differ from the
        // person whose refusal is under appeal.
        if rejected_by == Some(decided_by) {
            return Err(PrivacyError::AppealSelfReview);
        }
        self.outcome = Some(outcome);
        self.decided_at = Some(now);
        self.decided_by = Some(decided_by);
        self.decision_reason = Some(reason);
        Ok(())
    }

    pub fn is_decided(&self) -> bool {
        self.decided_at.is_some()
    }
}

/// A consent/directive ledger entry.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConsentRecord {
    #[schema(value_type = String)]
    pub id: ConsentId,
    #[schema(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
    #[schema(value_type = Option<String>)]
    pub subject_id: Option<SubjectId>,
    pub subject_ref: SubjectRef,
    pub purpose: super::types::ProcessingPurpose,
    pub granted: bool,
    pub notice_version: String,
    pub method: super::types::ConsentMethod,
    pub evidence: serde_json::Value,
    pub locale: Option<String>,
    pub jurisdiction: JurisdictionRef,
    pub signal_source: Option<super::types::SignalSource>,
    pub recorded_at: DateTime<Utc>,
    pub withdrawn_at: Option<DateTime<Utc>>,
}

/// A pre-authentication preference signal (GPC/UOOM).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PreferenceSignal {
    #[schema(value_type = String)]
    pub id: PreferenceSignalId,
    #[schema(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
    pub signal: String,
    pub device_ref: String,
    #[schema(value_type = Option<String>)]
    pub subject_id: Option<SubjectId>,
    pub jurisdiction: Option<JurisdictionRef>,
    pub purposes: Vec<super::types::ProcessingPurpose>,
    pub observed_at: DateTime<Utc>,
    pub bound_at: Option<DateTime<Utc>>,
    pub evidence: serde_json::Value,
}

/// A retention rule (one row of the data map's operational half).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RetentionRule {
    pub data_class: super::types::DataClass,
    pub sensitivity: super::types::SensitivityClass,
    pub basis: Option<super::types::LegalBasis>,
    pub period_days: i64,
    pub action: super::types::RetentionAction,
    pub disclosed_in_notice: bool,
}

/// A legal hold: blocks erasure for the named scope until released.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LegalHold {
    #[schema(value_type = String)]
    pub id: LegalHoldId,
    pub scope: serde_json::Value,
    pub reason: String,
    #[schema(value_type = String)]
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub released_by: Option<UserId>,
}

/// A sub-processor registry row. `role` is the field that turns an
/// ad-platform upload into a "sale"/"share" under US law — it is required
/// and asserted by phase 10's uploads.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SubProcessor {
    pub name: String,
    pub purpose: String,
    pub location: String,
    pub role: SubProcessorRole,
    pub contract_terms: serde_json::Value,
    pub dpa_url: Option<String>,
    pub transfer_mechanism: Option<TransferMechanism>,
    pub added_at: DateTime<Utc>,
    pub removed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubProcessorRole {
    Processor,
    ServiceProvider,
    Contractor,
    ThirdParty,
}

impl SubProcessorRole {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "processor" => Ok(Self::Processor),
            "service_provider" => Ok(Self::ServiceProvider),
            "contractor" => Ok(Self::Contractor),
            "third_party" => Ok(Self::ThirdParty),
            other => Err(PrivacyError::InvalidSubProcessorRole(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Processor => "processor",
            Self::ServiceProvider => "service_provider",
            Self::Contractor => "contractor",
            Self::ThirdParty => "third_party",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferMechanism {
    Sccs,
    UkAddendum,
    Dpf,
    AppiEquivalent,
    None,
}

impl TransferMechanism {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "sccs" => Ok(Self::Sccs),
            "uk_addendum" => Ok(Self::UkAddendum),
            "dpf" => Ok(Self::Dpf),
            "appi_equivalent" => Ok(Self::AppiEquivalent),
            "none" => Ok(Self::None),
            other => Err(PrivacyError::InvalidTransferMechanism(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sccs => "sccs",
            Self::UkAddendum => "uk_addendum",
            Self::Dpf => "dpf",
            Self::AppiEquivalent => "appi_equivalent",
            Self::None => "none",
        }
    }
}

/// A breach incident aggregate; obligations are computed per jurisdiction.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BreachIncident {
    #[schema(value_type = String)]
    pub id: BreachIncidentId,
    pub discovered_at: DateTime<Utc>,
    pub contained_at: Option<DateTime<Utc>>,
    pub affected: AffectedSet,
    pub data_classes: Vec<super::types::DataClass>,
    pub encrypted_at_rest: bool,
    pub notes: Option<String>,
    #[schema(value_type = Option<String>)]
    pub created_by: Option<UserId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct AffectedSet {
    pub subjects: u64,
    pub tenants: u64,
    pub jurisdictions: Vec<JurisdictionRef>,
}

/// One computed notification obligation.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NotificationObligation {
    pub jurisdiction: JurisdictionRef,
    pub audience: ObligationAudience,
    pub due_at: DateTime<Utc>,
    pub notified_at: Option<DateTime<Utc>>,
    pub suppressed_by_encryption: bool,
    pub artifact: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObligationAudience {
    Authority,
    Consumer,
    Tenant,
}

impl ObligationAudience {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authority => "authority",
            Self::Consumer => "consumer",
            Self::Tenant => "tenant",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "authority" => Ok(Self::Authority),
            "consumer" => Ok(Self::Consumer),
            "tenant" => Ok(Self::Tenant),
            other => Err(PrivacyError::InvalidObligationAudience(other.to_string())),
        }
    }
}

/// An export artifact: packaged, encrypted, TTL-bound.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ExportArtifact {
    #[schema(value_type = String)]
    pub id: ExportArtifactId,
    #[schema(value_type = String)]
    pub request_id: DsrId,
    #[schema(value_type = String)]
    pub subject_id: SubjectId,
    pub download_token_hash: String,
    pub passphrase_verifier: String,
    pub manifest_sha256: String,
    pub path: String,
    pub content_size: i64,
    pub expires_at: DateTime<Utc>,
    pub max_downloads: i32,
    pub download_count: i32,
    pub downloaded_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// A verification challenge (email OTP).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[schema(as = PrivacyVerificationChallenge)]
pub struct VerificationChallenge {
    #[schema(value_type = String)]
    pub id: VerificationChallengeId,
    #[schema(value_type = String)]
    pub request_id: DsrId,
    pub method: String,
    pub token_hash: String,
    pub code_hash: String,
    pub expires_at: DateTime<Utc>,
    pub attempts: i32,
    pub consumed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// A CCPA authorized-agent submission.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AuthorizedAgent {
    #[schema(value_type = String)]
    pub id: AuthorizedAgentId,
    #[schema(value_type = String)]
    pub request_id: DsrId,
    pub agent_name: String,
    pub agent_contact: Option<serde_json::Value>,
    pub authorization_evidence: serde_json::Value,
    pub consumer_verified_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub verified_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

/// An Art. 30 processing record (versioned; the active row describes the
/// current processing).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProcessingActivity {
    #[schema(value_type = String)]
    pub id: ProcessingActivityId,
    pub name: String,
    pub purpose: String,
    pub data_classes: Vec<super::types::DataClass>,
    pub controller_role: ControllerRole,
    pub basis: Option<String>,
    pub version: i32,
    #[schema(value_type = Option<String>)]
    pub supersedes: Option<ProcessingActivityId>,
    pub active: bool,
    #[schema(value_type = Option<String>)]
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControllerRole {
    Controller,
    Processor,
    Business,
    ServiceProvider,
}

impl ControllerRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Controller => "controller",
            Self::Processor => "processor",
            Self::Business => "business",
            Self::ServiceProvider => "service_provider",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "controller" => Ok(Self::Controller),
            "processor" => Ok(Self::Processor),
            "business" => Ok(Self::Business),
            "service_provider" => Ok(Self::ServiceProvider),
            other => Err(PrivacyError::InvalidRequester(format!(
                "invalid controller role: {other}"
            ))),
        }
    }
}

/// A DPIA / US state data-protection assessment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RiskAssessment {
    #[schema(value_type = String)]
    pub id: RiskAssessmentId,
    #[schema(value_type = String)]
    pub activity_id: ProcessingActivityId,
    pub jurisdictions: Vec<JurisdictionRef>,
    pub trigger: RiskAssessmentTrigger,
    pub status: RiskAssessmentStatus,
    pub findings: Option<serde_json::Value>,
    pub mitigations: Option<serde_json::Value>,
    #[schema(value_type = Option<String>)]
    pub reviewed_by: Option<UserId>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub next_review_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RiskAssessmentTrigger {
    TargetedAds,
    Sale,
    Sensitive,
    Profiling,
    LargeScale,
    HealthData,
}

impl RiskAssessmentTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TargetedAds => "targeted_ads",
            Self::Sale => "sale",
            Self::Sensitive => "sensitive",
            Self::Profiling => "profiling",
            Self::LargeScale => "large_scale",
            Self::HealthData => "health_data",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "targeted_ads" => Ok(Self::TargetedAds),
            "sale" => Ok(Self::Sale),
            "sensitive" => Ok(Self::Sensitive),
            "profiling" => Ok(Self::Profiling),
            "large_scale" => Ok(Self::LargeScale),
            "health_data" => Ok(Self::HealthData),
            other => Err(PrivacyError::InvalidRequester(format!(
                "invalid risk assessment trigger: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RiskAssessmentStatus {
    Open,
    InReview,
    Approved,
    Superseded,
}

impl RiskAssessmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InReview => "in_review",
            Self::Approved => "approved",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "open" => Ok(Self::Open),
            "in_review" => Ok(Self::InReview),
            "approved" => Ok(Self::Approved),
            "superseded" => Ok(Self::Superseded),
            other => Err(PrivacyError::InvalidRequester(format!(
                "invalid risk assessment status: {other}"
            ))),
        }
    }
}
