//! The sqlx privacy repositories. Tenant-scoped reads that originate from a
//! tenant handler run under `ScopedTx` (RLS-filtered by construction);
//! writes and cross-tenant maintenance run under the audited
//! `PlatformScope`. Plain `sqlx::query` calls (no compile-time macros), so
//! the offline `.sqlx` cache never needs regeneration for this context.

use std::collections::BTreeMap;
use std::net::IpAddr;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{PlatformScope, ScopedTx};
use sqlx::FromRow;

use crate::domain::{
    Appeal, AppealId, AppealOutcome, AppealRepository, AuthorizedAgent, AuthorizedAgentId,
    AuthorizedAgentRepository, BreachIncident, BreachIncidentId, BreachRepository, BreachRuleSet,
    ConsentId, ConsentMethod, ConsentRecord, ConsentRepository, ControllerRole, DataSubject,
    DataSubjectRequest, DirectiveRepository, DsrId, DsrKind, DsrRepository, DsrStatus,
    ExportArtifact, ExportArtifactId, ExportArtifactRepository, JurisdictionProfile,
    JurisdictionRef, JurisdictionRepository, LegalHold, LegalHoldId, LegalHoldRepository,
    NotificationObligation, ObligationAudience, ParticipantRepository, ParticipantRow,
    ParticipantStatus, PreferenceSignal, PreferenceSignalId, PrivacyError, ProcessingActivity,
    ProcessingActivityId, ProcessingActivityRepository, ProcessingPurpose, PurposeDirective,
    Regime, RequestMetricRow, RequestMetricsRepository, RetentionAction, RetentionRule,
    RetentionRuleRepository, RiskAssessment, RiskAssessmentId, RiskAssessmentRepository,
    RiskAssessmentStatus, RiskAssessmentTrigger, SensitivityClass, SignalRepository, SignalSource,
    SubProcessor, SubProcessorRepository, SubProcessorRole, SubjectId, SubjectKeyRepository,
    SubjectKeyRow, SubjectKind, SubjectRef, SubjectRepository, TransferMechanism,
    VerificationChallenge, VerificationChallengeId, VerificationRepository, YearlyMetricRow,
};

fn map_db(err: sqlx::Error) -> PrivacyError {
    PrivacyError::Storage(err.to_string())
}

fn invalid_id(kind: &str, err: impl std::fmt::Display) -> PrivacyError {
    PrivacyError::Storage(format!("invalid {kind} id: {err}"))
}

/// Reserved node-zero seed actor: attributed to system-initiated writes with
/// no human actor (e.g. seeding the compile-time data map's operational
/// retention rules at boot). Node zero is reserved for stable seed
/// constants and never collides with a runtime-generated Snowflake.
fn system_actor() -> UserId {
    UserId::try_from(1_i64).expect("system actor id is a valid seed constant")
}

// ---------------------------------------------------------------------------
// Shared row→domain helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct SubjectRow {
    id: i64,
    tenant_id: Option<i64>,
    kind: String,
    canonical_key: String,
    identifiers: serde_json::Value,
    jurisdiction: String,
    jurisdiction_evidence: serde_json::Value,
    is_minor: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn to_subject(row: SubjectRow) -> Result<DataSubject, PrivacyError> {
    let tenant_id = row
        .tenant_id
        .map(TenantId::try_from)
        .transpose()
        .map_err(|err| invalid_id("tenant", err))?;
    Ok(DataSubject {
        id: SubjectId::try_from(row.id).map_err(|err| invalid_id("subject", err))?,
        tenant_id,
        subject_ref: SubjectRef {
            kind: SubjectKind::parse(&row.kind)?,
            tenant_id,
            key: row.canonical_key,
            identifiers: serde_json::from_value(row.identifiers).unwrap_or_default(),
        },
        jurisdiction: JurisdictionRef::parse(&row.jurisdiction)?,
        jurisdiction_evidence: serde_json::from_value(row.jurisdiction_evidence)
            .unwrap_or_default(),
        is_minor: row.is_minor,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

#[derive(Debug, Clone, FromRow)]
struct DsrRow {
    id: i64,
    tenant_id: Option<i64>,
    subject_id: i64,
    subject_ref: serde_json::Value,
    kind: String,
    status: String,
    received_at: DateTime<Utc>,
    acknowledged_at: Option<DateTime<Utc>>,
    verified_at: Option<DateTime<Utc>>,
    due_at: DateTime<Utc>,
    extended_to: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    jurisdiction: String,
    submitted_by: String,
    agent_id: Option<i64>,
    requester_ip: Option<IpAddr>,
    requester_note: Option<String>,
    rejection_reason: Option<String>,
    rejected_by: Option<i64>,
    appeal_id: Option<i64>,
    export_artifact_id: Option<i64>,
}

fn to_dsr(row: DsrRow) -> Result<DataSubjectRequest, PrivacyError> {
    let subject_ref: SubjectRef = serde_json::from_value(row.subject_ref)
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
    Ok(DataSubjectRequest {
        id: DsrId::try_from(row.id).map_err(|err| invalid_id("dsr", err))?,
        tenant_id: row
            .tenant_id
            .map(TenantId::try_from)
            .transpose()
            .map_err(|err| invalid_id("tenant", err))?,
        subject_id: SubjectId::try_from(row.subject_id)
            .map_err(|err| invalid_id("subject", err))?,
        subject_ref,
        kind: DsrKind::parse(&row.kind)?,
        status: DsrStatus::parse(&row.status)?,
        received_at: row.received_at,
        acknowledged_at: row.acknowledged_at,
        verified_at: row.verified_at,
        due_at: row.due_at,
        extended_to: row.extended_to,
        completed_at: row.completed_at,
        jurisdiction: JurisdictionRef::parse(&row.jurisdiction)?,
        submitted_by: super::super::domain::types::Requester::parse(&row.submitted_by)?,
        agent_id: row
            .agent_id
            .map(AuthorizedAgentId::try_from)
            .transpose()
            .map_err(|err| invalid_id("authorized agent", err))?,
        requester_ip: row.requester_ip,
        requester_note: row.requester_note,
        rejection_reason: row.rejection_reason,
        rejected_by: row
            .rejected_by
            .map(UserId::try_from)
            .transpose()
            .map_err(|err| invalid_id("user", err))?,
        appeal_id: row
            .appeal_id
            .map(AppealId::try_from)
            .transpose()
            .map_err(|err| invalid_id("appeal", err))?,
        export_artifact_id: row
            .export_artifact_id
            .map(ExportArtifactId::try_from)
            .transpose()
            .map_err(|err| invalid_id("export artifact", err))?,
    })
}

#[derive(Debug, Clone, FromRow)]
struct AppealRow {
    id: i64,
    request_id: i64,
    reason: String,
    received_at: DateTime<Utc>,
    due_at: DateTime<Utc>,
    outcome: Option<String>,
    decided_at: Option<DateTime<Utc>>,
    decided_by: Option<i64>,
    decision_reason: Option<String>,
    authority_notice: Option<serde_json::Value>,
}

fn to_appeal(row: AppealRow) -> Result<Appeal, PrivacyError> {
    Ok(Appeal {
        id: AppealId::try_from(row.id).map_err(|err| invalid_id("appeal", err))?,
        request_id: DsrId::try_from(row.request_id).map_err(|err| invalid_id("dsr", err))?,
        reason: row.reason,
        received_at: row.received_at,
        due_at: row.due_at,
        outcome: match row.outcome.as_deref() {
            None => None,
            Some("upheld") => Some(AppealOutcome::Upheld),
            Some("denied") => Some(AppealOutcome::Denied),
            Some(other) => {
                return Err(PrivacyError::Conflict(format!(
                    "unknown appeal outcome: {other}"
                )));
            }
        },
        decided_at: row.decided_at,
        decided_by: row
            .decided_by
            .map(UserId::try_from)
            .transpose()
            .map_err(|err| invalid_id("user", err))?,
        decision_reason: row.decision_reason,
        authority_notice: row
            .authority_notice
            .and_then(|v| serde_json::from_value(v).ok()),
    })
}

// ---------------------------------------------------------------------------
// Jurisdictions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct JurisdictionRow {
    code: String,
    regime: String,
    consent_model: String,
    sensitive_model: String,
    ack_days: Option<i16>,
    response_days: i16,
    extension_days: i16,
    opt_out_effective_business_days: Option<i16>,
    appeal_window_days: Option<i16>,
    appeal_response_days: Option<i16>,
    honours_universal_opt_out: bool,
    minor_opt_in_age: Option<i16>,
    risk_assessment_required: bool,
    breach_rules: serde_json::Value,
    authority: serde_json::Value,
    effective_from: NaiveDate,
    source_note: Option<String>,
}

fn to_profile(row: JurisdictionRow) -> Result<JurisdictionProfile, PrivacyError> {
    let breach_rules: BreachRuleSet = serde_json::from_value(row.breach_rules)
        .map_err(|err| PrivacyError::Storage(format!("breach_rules: {err}")))?;
    let authority = serde_json::from_value(row.authority)
        .map_err(|err| PrivacyError::Storage(format!("authority: {err}")))?;
    Ok(JurisdictionProfile {
        code: JurisdictionRef::parse(&row.code)?,
        regime: Regime::parse(&row.regime)?,
        consent_model: super::super::domain::types::ConsentModel::parse(&row.consent_model)?,
        sensitive_model: super::super::domain::types::SensitiveModel::parse(&row.sensitive_model)?,
        ack_days: row.ack_days.map(|v| v as u16),
        response_days: row.response_days as u16,
        extension_days: row.extension_days as u16,
        opt_out_effective_business_days: row.opt_out_effective_business_days.map(|v| v as u16),
        appeal_window_days: row.appeal_window_days.map(|v| v as u16),
        appeal_response_days: row.appeal_response_days.map(|v| v as u16),
        honours_universal_opt_out: row.honours_universal_opt_out,
        minor_opt_in_age: row.minor_opt_in_age.map(|v| v as u8),
        risk_assessment_required: row.risk_assessment_required,
        breach_rules,
        authority,
        effective_from: row.effective_from,
        source_note: row.source_note,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxJurisdictionRepository;

impl SqlxJurisdictionRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl JurisdictionRepository for SqlxJurisdictionRepository {
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        code: &str,
    ) -> Result<Option<JurisdictionProfile>, PrivacyError> {
        let row: Option<JurisdictionRow> = sqlx::query_as(
            "SELECT code, regime, consent_model, sensitive_model, ack_days, response_days,
                    extension_days, opt_out_effective_business_days, appeal_window_days,
                    appeal_response_days, honours_universal_opt_out, minor_opt_in_age,
                    risk_assessment_required, breach_rules, authority, effective_from, source_note
             FROM privacy.jurisdiction_profiles WHERE code = $1",
        )
        .bind(code)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_profile).transpose()
    }

    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<JurisdictionProfile>, PrivacyError> {
        let rows: Vec<JurisdictionRow> = sqlx::query_as(
            "SELECT code, regime, consent_model, sensitive_model, ack_days, response_days,
                    extension_days, opt_out_effective_business_days, appeal_window_days,
                    appeal_response_days, honours_universal_opt_out, minor_opt_in_age,
                    risk_assessment_required, breach_rules, authority, effective_from, source_note
             FROM privacy.jurisdiction_profiles ORDER BY code",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_profile).collect()
    }

    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        profile: &JurisdictionProfile,
        updated_by: UserId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.jurisdiction_profiles
               (code, regime, consent_model, sensitive_model, ack_days, response_days,
                extension_days, opt_out_effective_business_days, appeal_window_days,
                appeal_response_days, honours_universal_opt_out, minor_opt_in_age,
                risk_assessment_required, breach_rules, authority, effective_from, source_note,
                updated_by, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, now())
             ON CONFLICT (code) DO UPDATE SET
               regime = EXCLUDED.regime,
               consent_model = EXCLUDED.consent_model,
               sensitive_model = EXCLUDED.sensitive_model,
               ack_days = EXCLUDED.ack_days,
               response_days = EXCLUDED.response_days,
               extension_days = EXCLUDED.extension_days,
               opt_out_effective_business_days = EXCLUDED.opt_out_effective_business_days,
               appeal_window_days = EXCLUDED.appeal_window_days,
               appeal_response_days = EXCLUDED.appeal_response_days,
               honours_universal_opt_out = EXCLUDED.honours_universal_opt_out,
               minor_opt_in_age = EXCLUDED.minor_opt_in_age,
               risk_assessment_required = EXCLUDED.risk_assessment_required,
               breach_rules = EXCLUDED.breach_rules,
               authority = EXCLUDED.authority,
               effective_from = EXCLUDED.effective_from,
               source_note = EXCLUDED.source_note,
               updated_by = EXCLUDED.updated_by,
               updated_at = now()",
        )
        .bind(profile.code.as_str())
        .bind(profile.regime.as_str())
        .bind(profile.consent_model.as_str())
        .bind(profile.sensitive_model.as_str())
        .bind(profile.ack_days.map(|v| v as i16))
        .bind(profile.response_days as i16)
        .bind(profile.extension_days as i16)
        .bind(profile.opt_out_effective_business_days.map(|v| v as i16))
        .bind(profile.appeal_window_days.map(|v| v as i16))
        .bind(profile.appeal_response_days.map(|v| v as i16))
        .bind(profile.honours_universal_opt_out)
        .bind(profile.minor_opt_in_age.map(|v| v as i16))
        .bind(profile.risk_assessment_required)
        .bind(serde_json::to_value(&profile.breach_rules).unwrap_or_default())
        .bind(serde_json::to_value(&profile.authority).unwrap_or_default())
        .bind(profile.effective_from)
        .bind(&profile.source_note)
        .bind(updated_by.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Subjects
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxSubjectRepository;

impl SqlxSubjectRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SubjectRepository for SqlxSubjectRepository {
    async fn find_by_key(
        &self,
        scope: &mut PlatformScope<'_>,
        key: &str,
    ) -> Result<Option<DataSubject>, PrivacyError> {
        let row: Option<SubjectRow> = sqlx::query_as(
            "SELECT id, tenant_id, kind, canonical_key, identifiers, jurisdiction,
                    jurisdiction_evidence, is_minor, created_at, updated_at
             FROM privacy.data_subjects WHERE canonical_key = $1",
        )
        .bind(key)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_subject).transpose()
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: SubjectId,
    ) -> Result<Option<DataSubject>, PrivacyError> {
        let row: Option<SubjectRow> = sqlx::query_as(
            "SELECT id, tenant_id, kind, canonical_key, identifiers, jurisdiction,
                    jurisdiction_evidence, is_minor, created_at, updated_at
             FROM privacy.data_subjects WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_subject).transpose()
    }

    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        subject: &DataSubject,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.data_subjects
               (id, tenant_id, kind, canonical_key, identifiers, jurisdiction,
                jurisdiction_evidence, is_minor, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE SET
               tenant_id = EXCLUDED.tenant_id,
               jurisdiction = EXCLUDED.jurisdiction,
               jurisdiction_evidence = EXCLUDED.jurisdiction_evidence,
               is_minor = EXCLUDED.is_minor,
               updated_at = now()",
        )
        .bind(subject.id.as_i64())
        .bind(subject.tenant_id.map(|id| id.as_i64()))
        .bind(subject.subject_ref.kind.as_str())
        .bind(subject.subject_ref.canonical_key())
        .bind(serde_json::to_value(&subject.subject_ref.identifiers).unwrap_or_default())
        .bind(subject.jurisdiction.as_str())
        .bind(serde_json::to_value(&subject.jurisdiction_evidence).unwrap_or_default())
        .bind(subject.is_minor)
        .bind(subject.created_at)
        .bind(subject.updated_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn set_minor(
        &self,
        scope: &mut PlatformScope<'_>,
        id: SubjectId,
        is_minor: bool,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.data_subjects SET is_minor = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id.as_i64())
        .bind(is_minor)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Data subject requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxDsrRepository;

impl SqlxDsrRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DsrRepository for SqlxDsrRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        request: &DataSubjectRequest,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.data_subject_requests
               (id, tenant_id, subject_id, subject_ref, kind, status, received_at,
                acknowledged_at, verified_at, due_at, extended_to, completed_at, jurisdiction,
                submitted_by, agent_id, requester_ip, requester_note, rejection_reason,
                rejected_by, appeal_id, export_artifact_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16::inet, $17, $18, $19, $20, $21)
             ON CONFLICT (id) DO UPDATE SET
               status = EXCLUDED.status,
               acknowledged_at = EXCLUDED.acknowledged_at,
               verified_at = EXCLUDED.verified_at,
               extended_to = EXCLUDED.extended_to,
               completed_at = EXCLUDED.completed_at,
               rejection_reason = EXCLUDED.rejection_reason,
               rejected_by = EXCLUDED.rejected_by,
               appeal_id = EXCLUDED.appeal_id,
               export_artifact_id = EXCLUDED.export_artifact_id,
               updated_at = now()",
        )
        .bind(request.id.as_i64())
        .bind(request.tenant_id.map(|id| id.as_i64()))
        .bind(request.subject_id.as_i64())
        .bind(serde_json::to_value(&request.subject_ref).map_err(|e| PrivacyError::Storage(e.to_string()))?)
        .bind(request.kind.as_str())
        .bind(request.status.as_str())
        .bind(request.received_at)
        .bind(request.acknowledged_at)
        .bind(request.verified_at)
        .bind(request.due_at)
        .bind(request.extended_to)
        .bind(request.completed_at)
        .bind(request.jurisdiction.as_str())
        .bind(request.submitted_by.as_str())
        .bind(request.agent_id.map(|id| id.as_i64()))
        .bind(request.requester_ip.map(|ip| ip.to_string()))
        .bind(&request.requester_note)
        .bind(&request.rejection_reason)
        .bind(request.rejected_by.map(|id| id.as_i64()))
        .bind(request.appeal_id.map(|id| id.as_i64()))
        .bind(request.export_artifact_id.map(|id| id.as_i64()))
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
    ) -> Result<Option<DataSubjectRequest>, PrivacyError> {
        let row: Option<DsrRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, kind, status, received_at, acknowledged_at, verified_at, due_at, extended_to, completed_at, jurisdiction, submitted_by, agent_id, requester_ip, requester_note, rejection_reason, rejected_by, appeal_id, export_artifact_id FROM privacy.data_subject_requests WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_dsr).transpose()
    }

    async fn find_by_id_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        id: DsrId,
    ) -> Result<Option<DataSubjectRequest>, PrivacyError> {
        let row: Option<DsrRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, kind, status, received_at, acknowledged_at, verified_at, due_at, extended_to, completed_at, jurisdiction, submitted_by, agent_id, requester_ip, requester_note, rejection_reason, rejected_by, appeal_id, export_artifact_id FROM privacy.data_subject_requests WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_dsr).transpose()
    }

    async fn find_by_subject_unverified(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<Vec<DataSubjectRequest>, PrivacyError> {
        let rows: Vec<DsrRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, kind, status, received_at, acknowledged_at, verified_at, due_at, extended_to, completed_at, jurisdiction, submitted_by, agent_id, requester_ip, requester_note, rejection_reason, rejected_by, appeal_id, export_artifact_id FROM privacy.data_subject_requests
             WHERE subject_id = $1 AND status = 'awaiting_verification'
             ORDER BY received_at DESC LIMIT 20",
        )
        .bind(subject_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_dsr).collect()
    }

    async fn open_requests(
        &self,
        scope: &mut PlatformScope<'_>,
        statuses: &[DsrStatus],
        kind: Option<DsrKind>,
        limit: u32,
    ) -> Result<Vec<DataSubjectRequest>, PrivacyError> {
        let statuses: Vec<String> = statuses.iter().map(|s| s.as_str().to_string()).collect();
        let rows: Vec<DsrRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, kind, status, received_at, acknowledged_at, verified_at, due_at, extended_to, completed_at, jurisdiction, submitted_by, agent_id, requester_ip, requester_note, rejection_reason, rejected_by, appeal_id, export_artifact_id FROM privacy.data_subject_requests
             WHERE status = ANY($1) AND ($2::text IS NULL OR kind = $2)
             ORDER BY received_at ASC LIMIT $3",
        )
        .bind(&statuses)
        .bind(kind.map(|k| k.as_str().to_string()))
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_dsr).collect()
    }

    async fn list_operator(
        &self,
        scope: &mut PlatformScope<'_>,
        status: Option<DsrStatus>,
        limit: u32,
    ) -> Result<Vec<DataSubjectRequest>, PrivacyError> {
        let rows: Vec<DsrRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, kind, status, received_at, acknowledged_at, verified_at, due_at, extended_to, completed_at, jurisdiction, submitted_by, agent_id, requester_ip, requester_note, rejection_reason, rejected_by, appeal_id, export_artifact_id FROM privacy.data_subject_requests
             WHERE ($1::text IS NULL OR status = $1)
             ORDER BY received_at DESC LIMIT $2",
        )
        .bind(status.map(|s| s.as_str().to_string()))
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_dsr).collect()
    }

    async fn list_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        status: Option<DsrStatus>,
        limit: u32,
    ) -> Result<Vec<DataSubjectRequest>, PrivacyError> {
        let rows: Vec<DsrRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, kind, status, received_at, acknowledged_at, verified_at, due_at, extended_to, completed_at, jurisdiction, submitted_by, agent_id, requester_ip, requester_note, rejection_reason, rejected_by, appeal_id, export_artifact_id FROM privacy.data_subject_requests
             WHERE tenant_id = $1 AND ($2::text IS NULL OR status = $2)
             ORDER BY received_at DESC LIMIT $3",
        )
        .bind(tenant_id.as_i64())
        .bind(status.map(|s| s.as_str().to_string()))
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_dsr).collect()
    }

    async fn set_status(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        status: DsrStatus,
        completed_at: Option<DateTime<Utc>>,
        rejection_reason: Option<String>,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.data_subject_requests
             SET status = $2, completed_at = $3, rejection_reason = COALESCE($4, rejection_reason),
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_i64())
        .bind(status.as_str())
        .bind(completed_at)
        .bind(rejection_reason)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn set_rejected_by(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        actor: UserId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.data_subject_requests
             SET rejected_by = $2, updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_i64())
        .bind(actor.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn extend(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        extended_to: DateTime<Utc>,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.data_subject_requests SET extended_to = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id.as_i64())
        .bind(extended_to)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn acknowledge(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        at: DateTime<Utc>,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.data_subject_requests SET acknowledged_at = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id.as_i64())
        .bind(at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn verify(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        at: DateTime<Utc>,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.data_subject_requests
             SET verified_at = $2, status = 'in_progress', updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_i64())
        .bind(at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn link_artifact(
        &self,
        scope: &mut PlatformScope<'_>,
        id: DsrId,
        artifact_id: ExportArtifactId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.data_subject_requests SET export_artifact_id = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id.as_i64())
        .bind(artifact_id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn has_open_erasure_for_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<bool, PrivacyError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM privacy.data_subject_requests
             WHERE tenant_id = $1 AND kind = 'erasure'
               AND status IN ('in_progress', 'partially_complete', 'on_hold', 'under_appeal')",
        )
        .bind(tenant_id.as_i64())
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(count > 0)
    }

    async fn metrics_by_kind(
        &self,
        scope: &mut PlatformScope<'_>,
        year: i32,
    ) -> Result<Vec<RequestMetricRow>, PrivacyError> {
        #[derive(Debug, FromRow)]
        struct MetricRow {
            jurisdiction: String,
            kind: String,
            received: i64,
            complied: i64,
            denied: i64,
            median_days: Option<f64>,
        }
        let rows: Vec<MetricRow> = sqlx::query_as(
            "SELECT jurisdiction, kind,
                    COUNT(*) AS received,
                    COUNT(*) FILTER (WHERE status = 'completed') AS complied,
                    COUNT(*) FILTER (WHERE status = 'rejected') AS denied,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY
                      EXTRACT(EPOCH FROM (completed_at - received_at)) / 86400.0) AS median_days
             FROM privacy.data_subject_requests
             WHERE EXTRACT(YEAR FROM received_at) = $1
             GROUP BY jurisdiction, kind
             ORDER BY jurisdiction, kind",
        )
        .bind(f64::from(year))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(rows
            .into_iter()
            .map(|row| RequestMetricRow {
                jurisdiction: row.jurisdiction,
                kind: row.kind,
                received: row.received,
                complied: row.complied,
                denied: row.denied,
                median_days: row.median_days,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Appeals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxAppealRepository;

impl SqlxAppealRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AppealRepository for SqlxAppealRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        appeal: &Appeal,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.request_appeals
               (id, request_id, reason, received_at, due_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (id) DO UPDATE SET
               reason = EXCLUDED.reason, due_at = EXCLUDED.due_at",
        )
        .bind(appeal.id.as_i64())
        .bind(appeal.request_id.as_i64())
        .bind(&appeal.reason)
        .bind(appeal.received_at)
        .bind(appeal.due_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find_by_request(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<Option<Appeal>, PrivacyError> {
        let row: Option<AppealRow> = sqlx::query_as(
            "SELECT id, request_id, reason, received_at, due_at, outcome, decided_at, decided_by,
                    decision_reason, authority_notice
             FROM privacy.request_appeals WHERE request_id = $1",
        )
        .bind(request_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_appeal).transpose()
    }

    async fn find_open(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<Appeal>, PrivacyError> {
        let rows: Vec<AppealRow> = sqlx::query_as(
            "SELECT id, request_id, reason, received_at, due_at, outcome, decided_at, decided_by,
                    decision_reason, authority_notice
             FROM privacy.request_appeals WHERE decided_at IS NULL ORDER BY received_at ASC LIMIT $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_appeal).collect()
    }

    async fn decide(
        &self,
        scope: &mut PlatformScope<'_>,
        appeal: &Appeal,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.request_appeals
             SET outcome = $2, decided_at = $3, decided_by = $4, decision_reason = $5,
                 authority_notice = $6
             WHERE id = $1",
        )
        .bind(appeal.id.as_i64())
        .bind(appeal.outcome.map(|o| match o {
            AppealOutcome::Upheld => "upheld",
            AppealOutcome::Denied => "denied",
        }))
        .bind(appeal.decided_at)
        .bind(appeal.decided_by.map(|id| id.as_i64()))
        .bind(&appeal.decision_reason)
        .bind(
            appeal
                .authority_notice
                .as_ref()
                .map(|a| serde_json::to_value(a).unwrap_or_default()),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Participants
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxParticipantRepository;

impl SqlxParticipantRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ParticipantRepository for SqlxParticipantRepository {
    async fn ensure_participants(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
        contexts: &[&'static str],
    ) -> Result<(), PrivacyError> {
        for context in contexts {
            sqlx::query(
                "INSERT INTO privacy.request_participants (request_id, context)
                 VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(request_id.as_i64())
            .bind(context)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        }
        Ok(())
    }

    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<BTreeMap<String, ParticipantRow>, PrivacyError> {
        #[derive(Debug, FromRow)]
        struct Row {
            context: String,
            status: String,
            report: Option<serde_json::Value>,
            attempts: i32,
            last_error: Option<String>,
            updated_at: DateTime<Utc>,
        }
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT context, status, report, attempts, last_error, updated_at
             FROM privacy.request_participants WHERE request_id = $1",
        )
        .bind(request_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        let mut map = BTreeMap::new();
        for row in rows {
            map.insert(
                row.context.clone(),
                ParticipantRow {
                    context: row.context,
                    status: ParticipantStatus::parse(&row.status)?,
                    report: row.report,
                    attempts: row.attempts,
                    last_error: row.last_error,
                    updated_at: row.updated_at,
                },
            );
        }
        Ok(map)
    }

    async fn set_status(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
        context: &str,
        status: ParticipantStatus,
        report: Option<serde_json::Value>,
        error: Option<String>,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.request_participants
             SET status = $3, report = $4, last_error = $5, updated_at = now()
             WHERE request_id = $1 AND context = $2",
        )
        .bind(request_id.as_i64())
        .bind(context)
        .bind(status.as_str())
        .bind(report)
        .bind(error)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn record_attempt(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
        context: &str,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.request_participants SET attempts = attempts + 1, updated_at = now()
             WHERE request_id = $1 AND context = $2",
        )
        .bind(request_id.as_i64())
        .bind(context)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Consents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct ConsentRow {
    id: i64,
    tenant_id: Option<i64>,
    subject_id: Option<i64>,
    subject_ref: serde_json::Value,
    purpose: String,
    granted: bool,
    notice_version: String,
    method: String,
    evidence: serde_json::Value,
    locale: Option<String>,
    jurisdiction: String,
    signal_source: Option<String>,
    recorded_at: DateTime<Utc>,
    withdrawn_at: Option<DateTime<Utc>>,
}

fn to_consent(row: ConsentRow) -> Result<ConsentRecord, PrivacyError> {
    Ok(ConsentRecord {
        id: ConsentId::try_from(row.id).map_err(|err| invalid_id("consent", err))?,
        tenant_id: row
            .tenant_id
            .map(TenantId::try_from)
            .transpose()
            .map_err(|err| invalid_id("tenant", err))?,
        subject_id: row
            .subject_id
            .map(SubjectId::try_from)
            .transpose()
            .map_err(|err| invalid_id("subject", err))?,
        subject_ref: serde_json::from_value(row.subject_ref)
            .map_err(|e| PrivacyError::Storage(e.to_string()))?,
        purpose: ProcessingPurpose::parse(&row.purpose)?,
        granted: row.granted,
        notice_version: row.notice_version,
        method: ConsentMethod::parse(&row.method)?,
        evidence: row.evidence,
        locale: row.locale,
        jurisdiction: JurisdictionRef::parse(&row.jurisdiction)?,
        signal_source: row
            .signal_source
            .map(|s| SignalSource::parse(&s))
            .transpose()?,
        recorded_at: row.recorded_at,
        withdrawn_at: row.withdrawn_at,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxConsentRepository;

impl SqlxConsentRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ConsentRepository for SqlxConsentRepository {
    async fn append(
        &self,
        scope: &mut PlatformScope<'_>,
        record: &ConsentRecord,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.consents
               (id, tenant_id, subject_id, subject_ref, purpose, granted, notice_version, method,
                evidence, locale, jurisdiction, signal_source, recorded_at, withdrawn_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(record.id.as_i64())
        .bind(record.tenant_id.map(|id| id.as_i64()))
        .bind(record.subject_id.map(|id| id.as_i64()))
        .bind(
            serde_json::to_value(&record.subject_ref)
                .map_err(|e| PrivacyError::Storage(e.to_string()))?,
        )
        .bind(record.purpose.as_str())
        .bind(record.granted)
        .bind(&record.notice_version)
        .bind(record.method.as_str())
        .bind(&record.evidence)
        .bind(&record.locale)
        .bind(record.jurisdiction.as_str())
        .bind(record.signal_source.map(|s| s.as_str()))
        .bind(record.recorded_at)
        .bind(record.withdrawn_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn latest_for_purpose(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: Option<SubjectId>,
        subject_key: &str,
        purpose: ProcessingPurpose,
    ) -> Result<Option<ConsentRecord>, PrivacyError> {
        let row: Option<ConsentRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, purpose, granted, notice_version,
                    method, evidence, locale, jurisdiction, signal_source, recorded_at, withdrawn_at
             FROM privacy.consents
             WHERE purpose = $1 AND (subject_id = $2 OR subject_ref->>'key' = $3)
             ORDER BY recorded_at DESC LIMIT 1",
        )
        .bind(purpose.as_str())
        .bind(subject_id.map(|id| id.as_i64()))
        .bind(subject_key)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_consent).transpose()
    }

    async fn list_for_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: Option<SubjectId>,
        subject_key: &str,
    ) -> Result<Vec<ConsentRecord>, PrivacyError> {
        let rows: Vec<ConsentRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, purpose, granted, notice_version,
                    method, evidence, locale, jurisdiction, signal_source, recorded_at, withdrawn_at
             FROM privacy.consents
             WHERE subject_id = $1 OR subject_ref->>'key' = $2
             ORDER BY recorded_at ASC",
        )
        .bind(subject_id.map(|id| id.as_i64()))
        .bind(subject_key)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_consent).collect()
    }

    async fn unpropagated_opt_outs(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<ConsentRecord>, PrivacyError> {
        let rows: Vec<ConsentRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, purpose, granted, notice_version,
                    method, evidence, locale, jurisdiction, signal_source, recorded_at, withdrawn_at
             FROM privacy.consents
             WHERE granted = false AND method IN ('opt_out', 'withdrawal')
               AND evidence->>'propagated_at' IS NULL
             ORDER BY recorded_at ASC LIMIT $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_consent).collect()
    }

    async fn mark_propagated(
        &self,
        scope: &mut PlatformScope<'_>,
        record_id: ConsentId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.consents
             SET evidence = evidence || jsonb_build_object('propagated_at', now()::text)
             WHERE id = $1",
        )
        .bind(record_id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn expired(
        &self,
        scope: &mut PlatformScope<'_>,
        older_than: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ConsentRecord>, PrivacyError> {
        let rows: Vec<ConsentRow> = sqlx::query_as(
            "SELECT id, tenant_id, subject_id, subject_ref, purpose, granted, notice_version,
                    method, evidence, locale, jurisdiction, signal_source, recorded_at, withdrawn_at
             FROM privacy.consents
             WHERE recorded_at < $1 ORDER BY recorded_at ASC LIMIT $2",
        )
        .bind(older_than)
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_consent).collect()
    }

    async fn delete(
        &self,
        scope: &mut PlatformScope<'_>,
        record_id: ConsentId,
    ) -> Result<(), PrivacyError> {
        sqlx::query("DELETE FROM privacy.consents WHERE id = $1")
            .bind(record_id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        Ok(())
    }

    async fn anonymise_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_key: &str,
        anonymised: &SubjectRef,
    ) -> Result<usize, PrivacyError> {
        let result = sqlx::query(
            "UPDATE privacy.consents
             SET subject_id = NULL,
                 tenant_id = NULL,
                 subject_ref = $2
             WHERE subject_ref->>'key' = $1",
        )
        .bind(subject_key)
        .bind(serde_json::to_value(anonymised).map_err(|e| PrivacyError::Storage(e.to_string()))?)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(result.rows_affected() as usize)
    }

    async fn anonymise_record(
        &self,
        scope: &mut PlatformScope<'_>,
        record_id: ConsentId,
        anonymised: &SubjectRef,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.consents
             SET subject_id = NULL, tenant_id = NULL, subject_ref = $2
             WHERE id = $1",
        )
        .bind(record_id.as_i64())
        .bind(serde_json::to_value(anonymised).map_err(|e| PrivacyError::Storage(e.to_string()))?)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Preference signals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct SignalRow {
    id: i64,
    tenant_id: Option<i64>,
    signal: String,
    device_ref: String,
    subject_id: Option<i64>,
    jurisdiction: Option<String>,
    purposes: Vec<String>,
    observed_at: DateTime<Utc>,
    bound_at: Option<DateTime<Utc>>,
    evidence: serde_json::Value,
}

fn to_signal(row: SignalRow) -> Result<PreferenceSignal, PrivacyError> {
    Ok(PreferenceSignal {
        id: PreferenceSignalId::try_from(row.id)
            .map_err(|err| invalid_id("preference signal", err))?,
        tenant_id: row
            .tenant_id
            .map(TenantId::try_from)
            .transpose()
            .map_err(|err| invalid_id("tenant", err))?,
        signal: row.signal,
        device_ref: row.device_ref,
        subject_id: row
            .subject_id
            .map(SubjectId::try_from)
            .transpose()
            .map_err(|err| invalid_id("subject", err))?,
        jurisdiction: row
            .jurisdiction
            .map(|j| JurisdictionRef::parse(&j))
            .transpose()?,
        purposes: row
            .purposes
            .iter()
            .map(|p| ProcessingPurpose::parse(p))
            .collect::<Result<_, _>>()?,
        observed_at: row.observed_at,
        bound_at: row.bound_at,
        evidence: row.evidence,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxSignalRepository;

impl SqlxSignalRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SignalRepository for SqlxSignalRepository {
    async fn append(
        &self,
        scope: &mut PlatformScope<'_>,
        signal: &PreferenceSignal,
    ) -> Result<(), PrivacyError> {
        let purposes: Vec<String> = signal
            .purposes
            .iter()
            .map(|p| p.as_str().to_string())
            .collect();
        sqlx::query(
            "INSERT INTO privacy.preference_signals
               (id, tenant_id, signal, device_ref, subject_id, jurisdiction, purposes, observed_at,
                bound_at, evidence)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(signal.id.as_i64())
        .bind(signal.tenant_id.map(|id| id.as_i64()))
        .bind(&signal.signal)
        .bind(&signal.device_ref)
        .bind(signal.subject_id.map(|id| id.as_i64()))
        .bind(signal.jurisdiction.as_ref().map(|j| j.as_str()))
        .bind(&purposes)
        .bind(signal.observed_at)
        .bind(signal.bound_at)
        .bind(&signal.evidence)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn latest_for_device(
        &self,
        scope: &mut PlatformScope<'_>,
        device_ref: &str,
    ) -> Result<Option<PreferenceSignal>, PrivacyError> {
        let row: Option<SignalRow> = sqlx::query_as(
            "SELECT id, tenant_id, signal, device_ref, subject_id, jurisdiction, purposes,
                    observed_at, bound_at, evidence
             FROM privacy.preference_signals WHERE device_ref = $1
             ORDER BY observed_at DESC LIMIT 1",
        )
        .bind(device_ref)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_signal).transpose()
    }

    async fn unbound_for_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject: &SubjectRef,
    ) -> Result<Vec<PreferenceSignal>, PrivacyError> {
        let identifier_values: Vec<String> = subject
            .identifiers
            .iter()
            .map(|id| id.value.clone())
            .collect();
        let rows: Vec<SignalRow> = sqlx::query_as(
            "SELECT id, tenant_id, signal, device_ref, subject_id, jurisdiction, purposes,
                    observed_at, bound_at, evidence
             FROM privacy.preference_signals
             WHERE bound_at IS NULL AND evidence->>'identifier' = ANY($1)
             ORDER BY observed_at ASC",
        )
        .bind(&identifier_values)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_signal).collect()
    }

    async fn bind(
        &self,
        scope: &mut PlatformScope<'_>,
        signal_id: PreferenceSignalId,
        subject_id: SubjectId,
        jurisdiction: &str,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.preference_signals
             SET bound_at = now(), subject_id = $2, jurisdiction = COALESCE(jurisdiction, $3)
             WHERE id = $1 AND bound_at IS NULL",
        )
        .bind(signal_id.as_i64())
        .bind(subject_id.as_i64())
        .bind(jurisdiction)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn bind_by_device(
        &self,
        scope: &mut PlatformScope<'_>,
        device_ref: &str,
        subject_id: SubjectId,
        jurisdiction: &str,
    ) -> Result<usize, PrivacyError> {
        let result = sqlx::query(
            "UPDATE privacy.preference_signals
             SET bound_at = now(), subject_id = $2,
                 jurisdiction = COALESCE(jurisdiction, $3)
             WHERE device_ref = $1 AND bound_at IS NULL",
        )
        .bind(device_ref)
        .bind(subject_id.as_i64())
        .bind(jurisdiction)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(result.rows_affected() as usize)
    }

    async fn list_bound_for_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<Vec<PreferenceSignal>, PrivacyError> {
        let rows: Vec<SignalRow> = sqlx::query_as(
            "SELECT id, tenant_id, signal, device_ref, subject_id, jurisdiction, purposes,
                    observed_at, bound_at, evidence
             FROM privacy.preference_signals
             WHERE subject_id = $1 ORDER BY observed_at ASC",
        )
        .bind(subject_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_signal).collect()
    }

    async fn expired(
        &self,
        scope: &mut PlatformScope<'_>,
        older_than: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<PreferenceSignal>, PrivacyError> {
        let rows: Vec<SignalRow> = sqlx::query_as(
            "SELECT id, tenant_id, signal, device_ref, subject_id, jurisdiction, purposes,
                    observed_at, bound_at, evidence
             FROM privacy.preference_signals
             WHERE observed_at < $1 ORDER BY observed_at ASC LIMIT $2",
        )
        .bind(older_than)
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_signal).collect()
    }

    async fn delete(
        &self,
        scope: &mut PlatformScope<'_>,
        signal_id: PreferenceSignalId,
    ) -> Result<(), PrivacyError> {
        sqlx::query("DELETE FROM privacy.preference_signals WHERE id = $1")
            .bind(signal_id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        Ok(())
    }

    async fn delete_unbound(
        &self,
        scope: &mut PlatformScope<'_>,
        subject: &SubjectRef,
    ) -> Result<usize, PrivacyError> {
        let identifier_values: Vec<String> = subject
            .identifiers
            .iter()
            .map(|id| id.value.clone())
            .collect();
        let result = sqlx::query(
            "DELETE FROM privacy.preference_signals
             WHERE bound_at IS NULL AND evidence->>'identifier' = ANY($1)",
        )
        .bind(&identifier_values)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(result.rows_affected() as usize)
    }
}

// ---------------------------------------------------------------------------
// Purpose directives (materialised)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
struct DirectiveRow {
    subject_ref: serde_json::Value,
    purpose: String,
    state: String,
    source: String,
    jurisdiction: String,
    notice_version: Option<String>,
    effective_at: DateTime<Utc>,
    superseded_at: Option<DateTime<Utc>>,
}

fn to_directive(row: DirectiveRow) -> Result<PurposeDirective, PrivacyError> {
    Ok(PurposeDirective {
        purpose: ProcessingPurpose::parse(&row.purpose)?,
        state: super::super::domain::types::DirectiveState::parse(&row.state)?,
        source: super::super::domain::types::DirectiveSource::parse(&row.source)?,
        jurisdiction: JurisdictionRef::parse(&row.jurisdiction)?,
        notice_version: row.notice_version,
        effective_at: row.effective_at,
        superseded_at: row.superseded_at,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxDirectiveRepository;

impl SqlxDirectiveRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DirectiveRepository for SqlxDirectiveRepository {
    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        directive: &PurposeDirective,
        subject: &SubjectRef,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.purpose_directives
               (subject_id, subject_ref, purpose, state, source, jurisdiction, notice_version,
                effective_at, superseded_at, tenant_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (subject_ref, purpose) DO UPDATE SET
               state = EXCLUDED.state,
               source = EXCLUDED.source,
               jurisdiction = EXCLUDED.jurisdiction,
               notice_version = EXCLUDED.notice_version,
               effective_at = EXCLUDED.effective_at,
               superseded_at = NULL",
        )
        .bind(subject.tenant_id.map(|id| id.as_i64()))
        .bind(serde_json::to_value(subject).map_err(|e| PrivacyError::Storage(e.to_string()))?)
        .bind(directive.purpose.as_str())
        .bind(directive.state.as_str())
        .bind(directive.source.as_str())
        .bind(directive.jurisdiction.as_str())
        .bind(&directive.notice_version)
        .bind(directive.effective_at)
        .bind(directive.superseded_at)
        .bind(subject.tenant_id.map(|id| id.as_i64()))
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_key: &str,
        purpose: ProcessingPurpose,
    ) -> Result<Option<PurposeDirective>, PrivacyError> {
        let row: Option<DirectiveRow> = sqlx::query_as(
            "SELECT subject_ref, purpose, state, source, jurisdiction, notice_version,
                    effective_at, superseded_at
             FROM privacy.purpose_directives
             WHERE subject_ref->>'key' = $1 AND purpose = $2",
        )
        .bind(subject_key)
        .bind(purpose.as_str())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_directive).transpose()
    }

    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_key: &str,
    ) -> Result<Vec<PurposeDirective>, PrivacyError> {
        let rows: Vec<DirectiveRow> = sqlx::query_as(
            "SELECT subject_ref, purpose, state, source, jurisdiction, notice_version,
                    effective_at, superseded_at
             FROM privacy.purpose_directives
             WHERE subject_ref->>'key' = $1 AND superseded_at IS NULL
             ORDER BY purpose",
        )
        .bind(subject_key)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_directive).collect()
    }

    async fn supersede_all(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_key: &str,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.purpose_directives SET superseded_at = now()
             WHERE subject_ref->>'key' = $1 AND superseded_at IS NULL",
        )
        .bind(subject_key)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Retention rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct RetentionRow {
    data_class: String,
    sensitivity: String,
    basis: Option<String>,
    period_days: i64,
    action: String,
    disclosed_in_notice: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxRetentionRuleRepository;

impl SqlxRetentionRuleRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RetentionRuleRepository for SqlxRetentionRuleRepository {
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<RetentionRule>, PrivacyError> {
        let rows: Vec<RetentionRow> = sqlx::query_as(
            "SELECT data_class, sensitivity, basis,
                    EXTRACT(EPOCH FROM period)::bigint / 86400 AS period_days,
                    action, disclosed_in_notice
             FROM privacy.retention_rules ORDER BY data_class",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter()
            .map(|row| {
                Ok(RetentionRule {
                    data_class: super::super::domain::types::DataClass::parse(&row.data_class)?,
                    sensitivity: SensitivityClass::parse(&row.sensitivity)?,
                    basis: row
                        .basis
                        .map(|b| super::super::domain::types::LegalBasis::parse(&b))
                        .transpose()?,
                    period_days: row.period_days,
                    action: RetentionAction::parse(&row.action)?,
                    disclosed_in_notice: row.disclosed_in_notice,
                })
            })
            .collect()
    }

    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        rule: &RetentionRule,
        updated_by: UserId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.retention_rules
               (data_class, sensitivity, basis, period, action, disclosed_in_notice, updated_by, updated_at)
             VALUES ($1, $2, $3, make_interval(days => $4::int), $5, $6, $7, now())
             ON CONFLICT (data_class) DO UPDATE SET
               sensitivity = EXCLUDED.sensitivity,
               basis = EXCLUDED.basis,
               period = make_interval(days => $4::int),
               action = EXCLUDED.action,
               disclosed_in_notice = EXCLUDED.disclosed_in_notice,
               updated_by = EXCLUDED.updated_by,
               updated_at = now()",
        )
        .bind(rule.data_class.as_str())
        .bind(rule.sensitivity.as_str())
        .bind(rule.basis.map(|b| b.as_str()))
        .bind(rule.period_days as i32)
        .bind(rule.action.as_str())
        .bind(rule.disclosed_in_notice)
        .bind(updated_by.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn ensure_seeded(&self, scope: &mut PlatformScope<'_>) -> Result<usize, PrivacyError> {
        let seeded = crate::domain::DATA_CLASSES;
        for def in seeded {
            self.upsert(
                scope,
                &RetentionRule {
                    data_class: def.class,
                    sensitivity: def.sensitivity,
                    basis: Some(def.basis),
                    period_days: def.period_days,
                    action: def.action,
                    disclosed_in_notice: true,
                },
                system_actor(),
            )
            .await?;
        }
        Ok(seeded.len())
    }
}

// ---------------------------------------------------------------------------
// Legal holds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct HoldRow {
    id: i64,
    scope: serde_json::Value,
    reason: String,
    created_by: i64,
    created_at: DateTime<Utc>,
    released_at: Option<DateTime<Utc>>,
    released_by: Option<i64>,
}

fn to_legal_hold(row: HoldRow) -> Result<LegalHold, PrivacyError> {
    Ok(LegalHold {
        id: LegalHoldId::try_from(row.id).map_err(|err| invalid_id("legal hold", err))?,
        scope: row.scope,
        reason: row.reason,
        created_by: UserId::try_from(row.created_by).map_err(|err| invalid_id("user", err))?,
        created_at: row.created_at,
        released_at: row.released_at,
        released_by: row
            .released_by
            .map(UserId::try_from)
            .transpose()
            .map_err(|err| invalid_id("user", err))?,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxLegalHoldRepository;

impl SqlxLegalHoldRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LegalHoldRepository for SqlxLegalHoldRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        hold: &LegalHold,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.legal_holds (id, scope, reason, created_by, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(hold.id.as_i64())
        .bind(&hold.scope)
        .bind(&hold.reason)
        .bind(hold.created_by.as_i64())
        .bind(hold.created_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn active_for_subject(
        &self,
        scope: &mut PlatformScope<'_>,
        subject: &SubjectRef,
    ) -> Result<Vec<LegalHold>, PrivacyError> {
        let rows: Vec<HoldRow> = sqlx::query_as(
            "SELECT id, scope, reason, created_by, created_at, released_at, released_by
             FROM privacy.legal_holds
             WHERE released_at IS NULL AND (
               scope->>'subject_key' = $1 OR scope->>'tenant_id' = $2)",
        )
        .bind(subject.canonical_key())
        .bind(subject.tenant_id.map(|id| id.to_string()))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_legal_hold).collect()
    }

    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        include_released: bool,
    ) -> Result<Vec<LegalHold>, PrivacyError> {
        let rows: Vec<HoldRow> = sqlx::query_as(
            "SELECT id, scope, reason, created_by, created_at, released_at, released_by
             FROM privacy.legal_holds
             WHERE ($1 OR released_at IS NULL)
             ORDER BY created_at DESC",
        )
        .bind(include_released)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_legal_hold).collect()
    }

    async fn release(
        &self,
        scope: &mut PlatformScope<'_>,
        id: LegalHoldId,
        released_by: UserId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.legal_holds SET released_at = now(), released_by = $2 WHERE id = $1 AND released_at IS NULL",
        )
        .bind(id.as_i64())
        .bind(released_by.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Sub-processors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct SubProcessorRow {
    name: String,
    purpose: String,
    location: String,
    role: String,
    contract_terms: serde_json::Value,
    dpa_url: Option<String>,
    transfer_mechanism: Option<String>,
    added_at: DateTime<Utc>,
    removed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxSubProcessorRepository;

impl SqlxSubProcessorRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SubProcessorRepository for SqlxSubProcessorRepository {
    async fn list_public(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<SubProcessor>, PrivacyError> {
        let rows: Vec<SubProcessorRow> = sqlx::query_as(
            "SELECT name, purpose, location, role, contract_terms, dpa_url, transfer_mechanism,
                    added_at, removed_at
             FROM privacy.subprocessors WHERE removed_at IS NULL ORDER BY name",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_subprocessor).collect()
    }

    async fn list_all(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<SubProcessor>, PrivacyError> {
        let rows: Vec<SubProcessorRow> = sqlx::query_as(
            "SELECT name, purpose, location, role, contract_terms, dpa_url, transfer_mechanism,
                    added_at, removed_at
             FROM privacy.subprocessors ORDER BY name",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_subprocessor).collect()
    }

    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        processor: &SubProcessor,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.subprocessors
               (name, purpose, location, role, contract_terms, dpa_url, transfer_mechanism, added_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (name) DO UPDATE SET
               purpose = EXCLUDED.purpose,
               location = EXCLUDED.location,
               role = EXCLUDED.role,
               contract_terms = EXCLUDED.contract_terms,
               dpa_url = EXCLUDED.dpa_url,
               transfer_mechanism = EXCLUDED.transfer_mechanism,
               removed_at = NULL",
        )
        .bind(&processor.name)
        .bind(&processor.purpose)
        .bind(&processor.location)
        .bind(processor.role.as_str())
        .bind(&processor.contract_terms)
        .bind(&processor.dpa_url)
        .bind(processor.transfer_mechanism.map(|m| m.as_str()))
        .bind(processor.added_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn remove(&self, scope: &mut PlatformScope<'_>, name: &str) -> Result<(), PrivacyError> {
        sqlx::query("UPDATE privacy.subprocessors SET removed_at = now() WHERE name = $1")
            .bind(name)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        Ok(())
    }
}

fn to_subprocessor(row: SubProcessorRow) -> Result<SubProcessor, PrivacyError> {
    Ok(SubProcessor {
        name: row.name,
        purpose: row.purpose,
        location: row.location,
        role: SubProcessorRole::parse(&row.role)?,
        contract_terms: row.contract_terms,
        dpa_url: row.dpa_url,
        transfer_mechanism: row
            .transfer_mechanism
            .map(|m| TransferMechanism::parse(&m))
            .transpose()?,
        added_at: row.added_at,
        removed_at: row.removed_at,
    })
}

// ---------------------------------------------------------------------------
// Breach incidents + obligations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct IncidentRow {
    id: i64,
    discovered_at: DateTime<Utc>,
    contained_at: Option<DateTime<Utc>>,
    affected: serde_json::Value,
    data_classes: Vec<String>,
    encrypted_at_rest: bool,
    notes: Option<String>,
    created_by: Option<i64>,
}

fn to_incident(row: IncidentRow) -> Result<BreachIncident, PrivacyError> {
    Ok(BreachIncident {
        id: BreachIncidentId::try_from(row.id).map_err(|err| invalid_id("breach incident", err))?,
        discovered_at: row.discovered_at,
        contained_at: row.contained_at,
        affected: serde_json::from_value(row.affected)
            .map_err(|e| PrivacyError::Storage(e.to_string()))?,
        data_classes: row
            .data_classes
            .iter()
            .map(|c| super::super::domain::types::DataClass::parse(c))
            .collect::<Result<_, _>>()?,
        encrypted_at_rest: row.encrypted_at_rest,
        notes: row.notes,
        created_by: row
            .created_by
            .map(UserId::try_from)
            .transpose()
            .map_err(|err| invalid_id("user", err))?,
    })
}

#[derive(Debug, Clone, FromRow)]
struct ObligationRow {
    jurisdiction: String,
    audience: String,
    due_at: DateTime<Utc>,
    notified_at: Option<DateTime<Utc>>,
    artifact: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxBreachRepository;

impl SqlxBreachRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BreachRepository for SqlxBreachRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        incident: &BreachIncident,
    ) -> Result<(), PrivacyError> {
        let classes: Vec<String> = incident
            .data_classes
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        sqlx::query(
            "INSERT INTO privacy.breach_incidents
               (id, discovered_at, contained_at, affected, data_classes, encrypted_at_rest, notes, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(incident.id.as_i64())
        .bind(incident.discovered_at)
        .bind(incident.contained_at)
        .bind(serde_json::to_value(&incident.affected).map_err(|e| PrivacyError::Storage(e.to_string()))?)
        .bind(&classes)
        .bind(incident.encrypted_at_rest)
        .bind(&incident.notes)
        .bind(incident.created_by.map(|id| id.as_i64()))
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: BreachIncidentId,
    ) -> Result<Option<BreachIncident>, PrivacyError> {
        let row: Option<IncidentRow> = sqlx::query_as(
            "SELECT id, discovered_at, contained_at, affected, data_classes, encrypted_at_rest,
                    notes, created_by
             FROM privacy.breach_incidents WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_incident).transpose()
    }

    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<BreachIncident>, PrivacyError> {
        let rows: Vec<IncidentRow> = sqlx::query_as(
            "SELECT id, discovered_at, contained_at, affected, data_classes, encrypted_at_rest,
                    notes, created_by
             FROM privacy.breach_incidents ORDER BY discovered_at DESC",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_incident).collect()
    }

    async fn save_obligation(
        &self,
        scope: &mut PlatformScope<'_>,
        incident_id: BreachIncidentId,
        obligation: &NotificationObligation,
    ) -> Result<(), PrivacyError> {
        let artifact = serde_json::json!({
            "suppressed_by_encryption": obligation.suppressed_by_encryption,
            "notes": obligation.artifact,
        });
        sqlx::query(
            "INSERT INTO privacy.breach_obligations
               (incident_id, jurisdiction, audience, due_at, notified_at, artifact)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (incident_id, jurisdiction, audience) DO UPDATE SET
               due_at = EXCLUDED.due_at, artifact = EXCLUDED.artifact",
        )
        .bind(incident_id.as_i64())
        .bind(obligation.jurisdiction.as_str())
        .bind(obligation.audience.as_str())
        .bind(obligation.due_at)
        .bind(obligation.notified_at)
        .bind(artifact)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn list_obligations(
        &self,
        scope: &mut PlatformScope<'_>,
        incident_id: BreachIncidentId,
    ) -> Result<Vec<NotificationObligation>, PrivacyError> {
        let rows: Vec<ObligationRow> = sqlx::query_as(
            "SELECT jurisdiction, audience, due_at, notified_at, artifact
             FROM privacy.breach_obligations WHERE incident_id = $1 ORDER BY due_at",
        )
        .bind(incident_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter()
            .map(|row| {
                Ok(NotificationObligation {
                    jurisdiction: JurisdictionRef::parse(&row.jurisdiction)?,
                    audience: ObligationAudience::parse(&row.audience)?,
                    due_at: row.due_at,
                    notified_at: row.notified_at,
                    suppressed_by_encryption: row
                        .artifact
                        .as_ref()
                        .and_then(|a| a.get("suppressed_by_encryption"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    artifact: row.artifact,
                })
            })
            .collect()
    }

    async fn mark_notified(
        &self,
        scope: &mut PlatformScope<'_>,
        incident_id: BreachIncidentId,
        jurisdiction: &str,
        audience: ObligationAudience,
        artifact: Option<serde_json::Value>,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.breach_obligations
             SET notified_at = now(), artifact = $4
             WHERE incident_id = $1 AND jurisdiction = $2 AND audience = $3",
        )
        .bind(incident_id.as_i64())
        .bind(jurisdiction)
        .bind(audience.as_str())
        .bind(artifact)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Subject keys, artifacts, verification, metrics, activities, assessments,
// agents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxSubjectKeyRepository;

impl SqlxSubjectKeyRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SubjectKeyRepository for SqlxSubjectKeyRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
        wrapped_dek: Vec<u8>,
        kek_id: &str,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.subject_keys (subject_id, wrapped_dek, kek_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (subject_id) DO NOTHING",
        )
        .bind(subject_id.as_i64())
        .bind(&wrapped_dek)
        .bind(kek_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<Option<SubjectKeyRow>, PrivacyError> {
        let row: Option<(Vec<u8>, String, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT wrapped_dek, kek_id, created_at, destroyed_at
             FROM privacy.subject_keys WHERE subject_id = $1",
        )
        .bind(subject_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(row.map(
            |(wrapped_dek, kek_id, created_at, destroyed_at)| SubjectKeyRow {
                subject_id,
                wrapped_dek,
                kek_id,
                created_at,
                destroyed_at,
            },
        ))
    }

    async fn destroy(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.subject_keys
             SET wrapped_dek = ''::bytea, destroyed_at = now()
             WHERE subject_id = $1 AND destroyed_at IS NULL",
        )
        .bind(subject_id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn exists(
        &self,
        scope: &mut PlatformScope<'_>,
        subject_id: SubjectId,
    ) -> Result<bool, PrivacyError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM privacy.subject_keys
                 WHERE subject_id = $1 AND destroyed_at IS NULL",
        )
        .bind(subject_id.as_i64())
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(count > 0)
    }
}

#[derive(Debug, Clone, FromRow)]
struct ArtifactRow {
    id: i64,
    request_id: i64,
    subject_id: i64,
    download_token_hash: String,
    passphrase_verifier: String,
    manifest_sha256: String,
    path: String,
    content_size: i64,
    expires_at: DateTime<Utc>,
    max_downloads: i32,
    download_count: i32,
    downloaded_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxExportArtifactRepository;

impl SqlxExportArtifactRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ExportArtifactRepository for SqlxExportArtifactRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        artifact: &ExportArtifact,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.export_artifacts
               (id, request_id, subject_id, download_token_hash, passphrase_verifier,
                manifest_sha256, path, content_size, expires_at, max_downloads, download_count,
                downloaded_at, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(artifact.id.as_i64())
        .bind(artifact.request_id.as_i64())
        .bind(artifact.subject_id.as_i64())
        .bind(&artifact.download_token_hash)
        .bind(&artifact.passphrase_verifier)
        .bind(&artifact.manifest_sha256)
        .bind(&artifact.path)
        .bind(artifact.content_size)
        .bind(artifact.expires_at)
        .bind(artifact.max_downloads)
        .bind(artifact.download_count)
        .bind(artifact.downloaded_at)
        .bind(artifact.created_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ExportArtifactId,
    ) -> Result<Option<ExportArtifact>, PrivacyError> {
        let row: Option<ArtifactRow> = sqlx::query_as(
            "SELECT id, request_id, subject_id, download_token_hash, passphrase_verifier,
                    manifest_sha256, path, content_size, expires_at, max_downloads, download_count,
                    downloaded_at, created_at
             FROM privacy.export_artifacts WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_artifact).transpose()
    }

    async fn find_by_request(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<Option<ExportArtifact>, PrivacyError> {
        let row: Option<ArtifactRow> = sqlx::query_as(
            "SELECT id, request_id, subject_id, download_token_hash, passphrase_verifier,
                    manifest_sha256, path, content_size, expires_at, max_downloads, download_count,
                    downloaded_at, created_at
             FROM privacy.export_artifacts WHERE request_id = $1",
        )
        .bind(request_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_artifact).transpose()
    }

    async fn record_download(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ExportArtifactId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.export_artifacts
             SET download_count = download_count + 1, downloaded_at = now()
             WHERE id = $1",
        )
        .bind(id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn expired(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<ExportArtifact>, PrivacyError> {
        let rows: Vec<ArtifactRow> = sqlx::query_as(
            "SELECT id, request_id, subject_id, download_token_hash, passphrase_verifier,
                    manifest_sha256, path, content_size, expires_at, max_downloads, download_count,
                    downloaded_at, created_at
             FROM privacy.export_artifacts WHERE expires_at < now() LIMIT $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_artifact).collect()
    }

    async fn delete(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ExportArtifactId,
    ) -> Result<(), PrivacyError> {
        sqlx::query("DELETE FROM privacy.export_artifacts WHERE id = $1")
            .bind(id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        Ok(())
    }
}

fn to_artifact(row: ArtifactRow) -> Result<ExportArtifact, PrivacyError> {
    Ok(ExportArtifact {
        id: ExportArtifactId::try_from(row.id).map_err(|err| invalid_id("export artifact", err))?,
        request_id: DsrId::try_from(row.request_id).map_err(|err| invalid_id("dsr", err))?,
        subject_id: SubjectId::try_from(row.subject_id)
            .map_err(|err| invalid_id("subject", err))?,
        download_token_hash: row.download_token_hash,
        passphrase_verifier: row.passphrase_verifier,
        manifest_sha256: row.manifest_sha256,
        path: row.path,
        content_size: row.content_size,
        expires_at: row.expires_at,
        max_downloads: row.max_downloads,
        download_count: row.download_count,
        downloaded_at: row.downloaded_at,
        created_at: row.created_at,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxVerificationRepository;

impl SqlxVerificationRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl VerificationRepository for SqlxVerificationRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        challenge: &VerificationChallenge,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.verification_challenges
               (id, request_id, method, token_hash, code_hash, expires_at, attempts, consumed_at, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(challenge.id.as_i64())
        .bind(challenge.request_id.as_i64())
        .bind(&challenge.method)
        .bind(&challenge.token_hash)
        .bind(&challenge.code_hash)
        .bind(challenge.expires_at)
        .bind(challenge.attempts)
        .bind(challenge.consumed_at)
        .bind(challenge.created_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find_by_request(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<Option<VerificationChallenge>, PrivacyError> {
        #[derive(Debug, FromRow)]
        struct Row {
            id: i64,
            request_id: i64,
            method: String,
            token_hash: String,
            code_hash: String,
            expires_at: DateTime<Utc>,
            attempts: i32,
            consumed_at: Option<DateTime<Utc>>,
            created_at: DateTime<Utc>,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT id, request_id, method, token_hash, code_hash, expires_at, attempts,
                    consumed_at, created_at
             FROM privacy.verification_challenges WHERE request_id = $1",
        )
        .bind(request_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(|row| {
            Ok(VerificationChallenge {
                id: VerificationChallengeId::try_from(row.id)
                    .map_err(|err| invalid_id("verification challenge", err))?,
                request_id: DsrId::try_from(row.request_id)
                    .map_err(|err| invalid_id("dsr", err))?,
                method: row.method,
                token_hash: row.token_hash,
                code_hash: row.code_hash,
                expires_at: row.expires_at,
                attempts: row.attempts,
                consumed_at: row.consumed_at,
                created_at: row.created_at,
            })
        })
        .transpose()
    }

    async fn record_attempt(
        &self,
        scope: &mut PlatformScope<'_>,
        id: VerificationChallengeId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.verification_challenges SET attempts = attempts + 1 WHERE id = $1",
        )
        .bind(id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn consume(
        &self,
        scope: &mut PlatformScope<'_>,
        id: VerificationChallengeId,
    ) -> Result<(), PrivacyError> {
        sqlx::query("UPDATE privacy.verification_challenges SET consumed_at = now() WHERE id = $1")
            .bind(id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxRequestMetricsRepository;

impl SqlxRequestMetricsRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RequestMetricsRepository for SqlxRequestMetricsRepository {
    async fn upsert_yearly(
        &self,
        scope: &mut PlatformScope<'_>,
        year: i32,
        rows: &[RequestMetricRow],
    ) -> Result<(), PrivacyError> {
        for row in rows {
            sqlx::query(
                "INSERT INTO privacy.request_metrics_yearly
                   (year, jurisdiction, kind, received, complied, denied, median_days, generated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now())
                 ON CONFLICT (year, jurisdiction, kind) DO UPDATE SET
                   received = EXCLUDED.received,
                   complied = EXCLUDED.complied,
                   denied = EXCLUDED.denied,
                   median_days = EXCLUDED.median_days,
                   generated_at = now()",
            )
            .bind(year)
            .bind(&row.jurisdiction)
            .bind(&row.kind)
            .bind(row.received)
            .bind(row.complied)
            .bind(row.denied)
            .bind(row.median_days)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        }
        Ok(())
    }

    async fn list_yearly(
        &self,
        scope: &mut PlatformScope<'_>,
        year: i32,
    ) -> Result<Vec<YearlyMetricRow>, PrivacyError> {
        #[derive(Debug, FromRow)]
        struct Row {
            year: i32,
            jurisdiction: String,
            kind: String,
            received: i32,
            complied: i32,
            denied: i32,
            median_days: Option<f64>,
        }
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT year, jurisdiction, kind, received, complied, denied, median_days
             FROM privacy.request_metrics_yearly WHERE year = $1 ORDER BY jurisdiction, kind",
        )
        .bind(year)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(rows
            .into_iter()
            .map(|row| YearlyMetricRow {
                year: row.year,
                jurisdiction: row.jurisdiction,
                kind: row.kind,
                received: row.received,
                complied: row.complied,
                denied: row.denied,
                median_days: row.median_days,
            })
            .collect())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxProcessingActivityRepository;

impl SqlxProcessingActivityRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProcessingActivityRepository for SqlxProcessingActivityRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        activity: &ProcessingActivity,
    ) -> Result<(), PrivacyError> {
        let classes: Vec<String> = activity
            .data_classes
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        sqlx::query(
            "INSERT INTO privacy.processing_activities
               (id, name, purpose, data_classes, controller_role, basis, version, supersedes,
                active, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(activity.id.as_i64())
        .bind(&activity.name)
        .bind(&activity.purpose)
        .bind(&classes)
        .bind(activity.controller_role.as_str())
        .bind(&activity.basis)
        .bind(activity.version)
        .bind(activity.supersedes.map(|id| id.as_i64()))
        .bind(activity.active)
        .bind(activity.created_by.map(|id| id.as_i64()))
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ProcessingActivityId,
    ) -> Result<Option<ProcessingActivity>, PrivacyError> {
        let row: Option<ActivityRow> = sqlx::query_as(
            "SELECT id, name, purpose, data_classes, controller_role, basis, version,
                        supersedes, active, created_by, created_at
                 FROM privacy.processing_activities WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_activity).transpose()
    }

    async fn list_active(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<ProcessingActivity>, PrivacyError> {
        let rows: Vec<ActivityRow> = sqlx::query_as(
            "SELECT id, name, purpose, data_classes, controller_role, basis, version,
                        supersedes, active, created_by, created_at
                 FROM privacy.processing_activities WHERE active ORDER BY name",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_activity).collect()
    }
}

type ActivityRow = (
    i64,
    String,
    String,
    Vec<String>,
    String,
    Option<String>,
    i32,
    Option<i64>,
    bool,
    Option<i64>,
    DateTime<Utc>,
);

fn to_activity(row: ActivityRow) -> Result<ProcessingActivity, PrivacyError> {
    let (
        id,
        name,
        purpose,
        data_classes,
        controller_role,
        basis,
        version,
        supersedes,
        active,
        created_by,
        created_at,
    ) = row;
    Ok(ProcessingActivity {
        id: ProcessingActivityId::try_from(id)
            .map_err(|err| invalid_id("processing activity", err))?,
        name,
        purpose,
        data_classes: data_classes
            .iter()
            .map(|c| super::super::domain::types::DataClass::parse(c))
            .collect::<Result<_, _>>()?,
        controller_role: ControllerRole::parse(&controller_role)?,
        basis,
        version,
        supersedes: supersedes
            .map(ProcessingActivityId::try_from)
            .transpose()
            .map_err(|err| invalid_id("processing activity", err))?,
        active,
        created_by: created_by
            .map(UserId::try_from)
            .transpose()
            .map_err(|err| invalid_id("user", err))?,
        created_at,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxRiskAssessmentRepository;

impl SqlxRiskAssessmentRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RiskAssessmentRepository for SqlxRiskAssessmentRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        assessment: &RiskAssessment,
    ) -> Result<(), PrivacyError> {
        let jurisdictions: Vec<String> = assessment
            .jurisdictions
            .iter()
            .map(|j| j.as_str().to_string())
            .collect();
        sqlx::query(
            "INSERT INTO privacy.risk_assessments
               (id, activity_id, jurisdictions, trigger, status, findings, mitigations,
                reviewed_by, reviewed_at, next_review_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE SET
               status = EXCLUDED.status,
               findings = EXCLUDED.findings,
               mitigations = EXCLUDED.mitigations,
               reviewed_by = EXCLUDED.reviewed_by,
               reviewed_at = EXCLUDED.reviewed_at,
               next_review_at = EXCLUDED.next_review_at",
        )
        .bind(assessment.id.as_i64())
        .bind(assessment.activity_id.as_i64())
        .bind(&jurisdictions)
        .bind(assessment.trigger.as_str())
        .bind(assessment.status.as_str())
        .bind(&assessment.findings)
        .bind(&assessment.mitigations)
        .bind(assessment.reviewed_by.map(|id| id.as_i64()))
        .bind(assessment.reviewed_at)
        .bind(assessment.next_review_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: RiskAssessmentId,
    ) -> Result<Option<RiskAssessment>, PrivacyError> {
        let row: Option<AssessmentRow> = sqlx::query_as(
            "SELECT id, activity_id, jurisdictions, trigger, status, findings, mitigations,
                        reviewed_by, reviewed_at, next_review_at, created_at
                 FROM privacy.risk_assessments WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_assessment).transpose()
    }

    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<RiskAssessment>, PrivacyError> {
        let rows: Vec<AssessmentRow> = sqlx::query_as(
            "SELECT id, activity_id, jurisdictions, trigger, status, findings, mitigations,
                        reviewed_by, reviewed_at, next_review_at, created_at
                 FROM privacy.risk_assessments ORDER BY created_at DESC",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_assessment).collect()
    }
}

type AssessmentRow = (
    i64,
    i64,
    Vec<String>,
    String,
    String,
    Option<serde_json::Value>,
    Option<serde_json::Value>,
    Option<i64>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
);

fn to_assessment(row: AssessmentRow) -> Result<RiskAssessment, PrivacyError> {
    let (
        id,
        activity_id,
        jurisdictions,
        trigger,
        status,
        findings,
        mitigations,
        reviewed_by,
        reviewed_at,
        next_review_at,
        created_at,
    ) = row;
    Ok(RiskAssessment {
        id: RiskAssessmentId::try_from(id).map_err(|err| invalid_id("risk assessment", err))?,
        activity_id: ProcessingActivityId::try_from(activity_id)
            .map_err(|err| invalid_id("processing activity", err))?,
        jurisdictions: jurisdictions
            .iter()
            .map(|j| JurisdictionRef::parse(j))
            .collect::<Result<_, _>>()?,
        trigger: RiskAssessmentTrigger::parse(&trigger)?,
        status: RiskAssessmentStatus::parse(&status)?,
        findings,
        mitigations,
        reviewed_by: reviewed_by
            .map(UserId::try_from)
            .transpose()
            .map_err(|err| invalid_id("user", err))?,
        reviewed_at,
        next_review_at,
        created_at,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxAuthorizedAgentRepository;

impl SqlxAuthorizedAgentRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AuthorizedAgentRepository for SqlxAuthorizedAgentRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        agent: &AuthorizedAgent,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "INSERT INTO privacy.authorized_agents
               (id, request_id, agent_name, agent_contact, authorization_evidence,
                consumer_verified_at, verified_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(agent.id.as_i64())
        .bind(agent.request_id.as_i64())
        .bind(&agent.agent_name)
        .bind(&agent.agent_contact)
        .bind(&agent.authorization_evidence)
        .bind(agent.consumer_verified_at)
        .bind(agent.verified_by.map(|id| id.as_i64()))
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find_by_request(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
    ) -> Result<Option<AuthorizedAgent>, PrivacyError> {
        let row: Option<(
            i64,
            i64,
            String,
            Option<serde_json::Value>,
            serde_json::Value,
            Option<DateTime<Utc>>,
            Option<i64>,
            DateTime<Utc>,
        )> = sqlx::query_as(
            "SELECT id, request_id, agent_name, agent_contact, authorization_evidence,
                        consumer_verified_at, verified_by, created_at
                 FROM privacy.authorized_agents WHERE request_id = $1",
        )
        .bind(request_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(
            |(
                id,
                request_id,
                agent_name,
                agent_contact,
                authorization_evidence,
                consumer_verified_at,
                verified_by,
                created_at,
            )| {
                Ok(AuthorizedAgent {
                    id: AuthorizedAgentId::try_from(id)
                        .map_err(|err| invalid_id("authorized agent", err))?,
                    request_id: DsrId::try_from(request_id)
                        .map_err(|err| invalid_id("dsr", err))?,
                    agent_name,
                    agent_contact,
                    authorization_evidence,
                    consumer_verified_at,
                    verified_by: verified_by
                        .map(UserId::try_from)
                        .transpose()
                        .map_err(|err| invalid_id("user", err))?,
                    created_at,
                })
            },
        )
        .transpose()
    }

    async fn mark_consumer_verified(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: DsrId,
        verified_by: UserId,
    ) -> Result<(), PrivacyError> {
        sqlx::query(
            "UPDATE privacy.authorized_agents
             SET consumer_verified_at = now(), verified_by = $2
             WHERE request_id = $1 AND consumer_verified_at IS NULL",
        )
        .bind(request_id.as_i64())
        .bind(verified_by.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }
}
