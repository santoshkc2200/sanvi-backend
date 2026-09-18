//! Governance: jurisdiction profiles, legal holds, sub-processors, breach
//! incidents with per-jurisdiction obligations, Art. 30 records and risk
//! assessments.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::UserId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sanvi_persistence::TxManager;

use crate::domain::{
    AffectedSet, AuditWriter, BreachIncident, BreachIncidentId, BreachObligationCalculator,
    BreachRepository, Clock, DataClass, EventPublisher, IdGenerator, JurisdictionProfile,
    JurisdictionRepository, LegalHold, LegalHoldId, LegalHoldRepository, NotificationObligation,
    PrivacyError, PrivacyEvent, ProcessingActivity, ProcessingActivityId,
    ProcessingActivityRepository, RiskAssessment, RiskAssessmentId, RiskAssessmentRepository,
    SubProcessor, SubProcessorNotifier, SubProcessorRepository,
};

// ---------------------------------------------------------------------------
// Jurisdiction profiles (audited configuration)
// ---------------------------------------------------------------------------

pub struct ListJurisdictions {
    tx_manager: TxManager,
    jurisdictions: Arc<dyn JurisdictionRepository>,
}

impl ListJurisdictions {
    pub fn new(tx_manager: TxManager, jurisdictions: Arc<dyn JurisdictionRepository>) -> Self {
        Self {
            tx_manager,
            jurisdictions,
        }
    }

    #[tracing::instrument(name = "privacy.list_jurisdictions", skip_all)]
    pub async fn execute(&self) -> Result<Vec<JurisdictionProfile>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_jurisdiction_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let profiles = self.jurisdictions.list(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(profiles)
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpsertJurisdictionCommand {
    pub code: String,
    pub regime: String,
    pub consent_model: String,
    pub sensitive_model: String,
    pub ack_days: Option<u16>,
    pub response_days: u16,
    pub extension_days: u16,
    pub opt_out_effective_business_days: Option<u16>,
    pub appeal_window_days: Option<u16>,
    pub appeal_response_days: Option<u16>,
    pub honours_universal_opt_out: bool,
    pub minor_opt_in_age: Option<u8>,
    pub risk_assessment_required: bool,
    pub breach_rules: crate::domain::BreachRuleSet,
    pub authority: crate::domain::AuthorityContact,
    pub source_note: Option<String>,
}

pub struct UpsertJurisdiction {
    tx_manager: TxManager,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    audit: Arc<dyn AuditWriter>,
    clock: Arc<dyn Clock>,
}

impl UpsertJurisdiction {
    pub fn new(
        tx_manager: TxManager,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        audit: Arc<dyn AuditWriter>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            jurisdictions,
            audit,
            clock,
        }
    }

    #[tracing::instrument(name = "privacy.upsert_jurisdiction", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        command: UpsertJurisdictionCommand,
    ) -> Result<(), PrivacyError> {
        let profile = JurisdictionProfile {
            code: crate::domain::JurisdictionRef::parse(&command.code)?,
            regime: crate::domain::Regime::parse(&command.regime)?,
            consent_model: crate::domain::ConsentModel::parse(&command.consent_model)?,
            sensitive_model: crate::domain::types::SensitiveModel::parse(&command.sensitive_model)?,
            ack_days: command.ack_days,
            response_days: command.response_days,
            extension_days: command.extension_days,
            opt_out_effective_business_days: command.opt_out_effective_business_days,
            appeal_window_days: command.appeal_window_days,
            appeal_response_days: command.appeal_response_days,
            honours_universal_opt_out: command.honours_universal_opt_out,
            minor_opt_in_age: command.minor_opt_in_age,
            risk_assessment_required: command.risk_assessment_required,
            breach_rules: command.breach_rules,
            authority: command.authority,
            effective_from: self.clock.now().date_naive(),
            source_note: command.source_note,
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_jurisdiction_upsert", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.jurisdictions
            .upsert(&mut scope, &profile, actor)
            .await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.jurisdiction.update".to_string(),
                profile.code.as_str().to_string(),
                serde_json::to_value(&profile).unwrap_or_default(),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Legal holds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ApplyLegalHoldCommand {
    pub scope: serde_json::Value,
    pub reason: String,
}

pub struct ApplyLegalHold {
    tx_manager: TxManager,
    holds: Arc<dyn LegalHoldRepository>,
    events: Arc<dyn EventPublisher>,
    audit: Arc<dyn AuditWriter>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl ApplyLegalHold {
    pub fn new(
        tx_manager: TxManager,
        holds: Arc<dyn LegalHoldRepository>,
        events: Arc<dyn EventPublisher>,
        audit: Arc<dyn AuditWriter>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            holds,
            events,
            audit,
            ids,
            clock,
        }
    }

    #[tracing::instrument(name = "privacy.apply_legal_hold", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        command: ApplyLegalHoldCommand,
    ) -> Result<LegalHoldId, PrivacyError> {
        if command.reason.trim().is_empty() {
            return Err(PrivacyError::Conflict("reason required".to_string()));
        }
        let hold = LegalHold {
            id: self.ids.next_legal_hold_id()?,
            scope: command.scope.clone(),
            reason: command.reason,
            created_by: actor,
            created_at: self.clock.now(),
            released_at: None,
            released_by: None,
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_legal_hold", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.holds.save(&mut scope, &hold).await?;
        self.events
            .publish(
                &mut scope,
                &[PrivacyEvent::LegalHoldApplied {
                    hold_id: hold.id,
                    scope: command.scope,
                }],
            )
            .await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.hold.apply".to_string(),
                hold.id.to_string(),
                serde_json::json!({ "reason": hold.reason }),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(hold.id)
    }
}

pub struct ReleaseLegalHold {
    tx_manager: TxManager,
    holds: Arc<dyn LegalHoldRepository>,
    events: Arc<dyn EventPublisher>,
    audit: Arc<dyn AuditWriter>,
}

impl ReleaseLegalHold {
    pub fn new(
        tx_manager: TxManager,
        holds: Arc<dyn LegalHoldRepository>,
        events: Arc<dyn EventPublisher>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            holds,
            events,
            audit,
        }
    }

    #[tracing::instrument(name = "privacy.release_legal_hold", skip_all)]
    pub async fn execute(&self, actor: UserId, hold_id: LegalHoldId) -> Result<(), PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_hold_release", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.holds.release(&mut scope, hold_id, actor).await?;
        self.events
            .publish(&mut scope, &[PrivacyEvent::LegalHoldReleased { hold_id }])
            .await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.hold.release".to_string(),
                hold_id.to_string(),
                serde_json::json!({}),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Sub-processors
// ---------------------------------------------------------------------------

pub struct ListSubProcessors {
    tx_manager: TxManager,
    processors: Arc<dyn SubProcessorRepository>,
}

impl ListSubProcessors {
    pub fn new(tx_manager: TxManager, processors: Arc<dyn SubProcessorRepository>) -> Self {
        Self {
            tx_manager,
            processors,
        }
    }

    #[tracing::instrument(name = "privacy.list_sub_processors", skip_all)]
    pub async fn execute(&self, include_removed: bool) -> Result<Vec<SubProcessor>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_subprocessor_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let processors = if include_removed {
            self.processors.list_all(&mut scope).await?
        } else {
            self.processors.list_public(&mut scope).await?
        };
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(processors)
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpsertSubProcessorCommand {
    pub name: String,
    pub purpose: String,
    pub location: String,
    pub role: String,
    pub contract_terms: serde_json::Value,
    pub dpa_url: Option<String>,
    pub transfer_mechanism: Option<String>,
}

pub struct UpsertSubProcessor {
    tx_manager: TxManager,
    processors: Arc<dyn SubProcessorRepository>,
    audit: Arc<dyn AuditWriter>,
    clock: Arc<dyn Clock>,
}

impl UpsertSubProcessor {
    pub fn new(
        tx_manager: TxManager,
        processors: Arc<dyn SubProcessorRepository>,
        audit: Arc<dyn AuditWriter>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            processors,
            audit,
            clock,
        }
    }

    #[tracing::instrument(name = "privacy.upsert_sub_processor", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        command: UpsertSubProcessorCommand,
    ) -> Result<(), PrivacyError> {
        let processor = SubProcessor {
            name: command.name.clone(),
            purpose: command.purpose,
            location: command.location,
            role: crate::domain::SubProcessorRole::parse(&command.role)?,
            contract_terms: command.contract_terms,
            dpa_url: command.dpa_url,
            transfer_mechanism: command
                .transfer_mechanism
                .as_deref()
                .map(crate::domain::TransferMechanism::parse)
                .transpose()?,
            added_at: self.clock.now(),
            removed_at: None,
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_subprocessor_upsert", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.processors.upsert(&mut scope, &processor).await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.subprocessor.update".to_string(),
                processor.name.clone(),
                serde_json::to_value(&processor).unwrap_or_default(),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

pub struct RemoveSubProcessor {
    tx_manager: TxManager,
    processors: Arc<dyn SubProcessorRepository>,
    notifier: Arc<dyn SubProcessorNotifier>,
    audit: Arc<dyn AuditWriter>,
}

impl RemoveSubProcessor {
    pub fn new(
        tx_manager: TxManager,
        processors: Arc<dyn SubProcessorRepository>,
        notifier: Arc<dyn SubProcessorNotifier>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            processors,
            notifier,
            audit,
        }
    }

    #[tracing::instrument(name = "privacy.remove_sub_processor", skip_all)]
    pub async fn execute(&self, actor: UserId, name: &str) -> Result<(), PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_subprocessor_remove", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.processors.remove(&mut scope, name).await?;
        let processors = self.processors.list_all(&mut scope).await?;
        if let Some(removed) = processors.iter().find(|p| p.name == name) {
            self.notifier.notify_removal(removed, vec![]).await?;
        }
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.subprocessor.remove".to_string(),
                name.to_string(),
                serde_json::json!({}),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Breach incidents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RecordBreachIncidentCommand {
    pub discovered_at: DateTime<Utc>,
    pub contained_at: Option<DateTime<Utc>>,
    pub subjects: u64,
    pub tenants: u64,
    pub jurisdictions: Vec<String>,
    pub data_classes: Vec<String>,
    pub encrypted_at_rest: bool,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RecordBreachIncidentOutput {
    #[schema(value_type = String)]
    pub incident_id: BreachIncidentId,
    pub obligations: Vec<NotificationObligation>,
}

pub struct RecordBreachIncident {
    tx_manager: TxManager,
    breaches: Arc<dyn BreachRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    calculator: Arc<dyn BreachObligationCalculator>,
    events: Arc<dyn EventPublisher>,
    audit: Arc<dyn AuditWriter>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

#[allow(clippy::too_many_arguments)]
impl RecordBreachIncident {
    pub fn new(
        tx_manager: TxManager,
        breaches: Arc<dyn BreachRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        calculator: Arc<dyn BreachObligationCalculator>,
        events: Arc<dyn EventPublisher>,
        audit: Arc<dyn AuditWriter>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            breaches,
            jurisdictions,
            calculator,
            events,
            audit,
            ids,
            clock,
        }
    }

    #[tracing::instrument(name = "privacy.record_breach_incident", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        command: RecordBreachIncidentCommand,
    ) -> Result<RecordBreachIncidentOutput, PrivacyError> {
        let jurisdictions = command
            .jurisdictions
            .iter()
            .map(|code| crate::domain::JurisdictionRef::parse(code))
            .collect::<Result<Vec<_>, _>>()?;
        let data_classes = command
            .data_classes
            .iter()
            .map(|class| DataClass::parse(class))
            .collect::<Result<Vec<_>, _>>()?;
        let incident = BreachIncident {
            id: self.ids.next_breach_incident_id()?,
            discovered_at: command.discovered_at,
            contained_at: command.contained_at,
            affected: AffectedSet {
                subjects: command.subjects,
                tenants: command.tenants,
                jurisdictions: jurisdictions.clone(),
            },
            data_classes,
            encrypted_at_rest: command.encrypted_at_rest,
            notes: command.notes,
            created_by: Some(actor),
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_breach_record", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let profiles = self.jurisdictions.list(&mut scope).await?;
        for jurisdiction in &jurisdictions {
            if !profiles.iter().any(|profile| profile.code == *jurisdiction) {
                return Err(PrivacyError::JurisdictionNotFound(
                    jurisdiction.as_str().to_string(),
                ));
            }
        }
        let obligations = self
            .calculator
            .compute(&incident, &profiles, self.clock.now());

        self.breaches.save(&mut scope, &incident).await?;
        for obligation in &obligations {
            self.breaches
                .save_obligation(&mut scope, incident.id, obligation)
                .await?;
        }
        self.events
            .publish(
                &mut scope,
                &[PrivacyEvent::BreachRecorded {
                    incident_id: incident.id,
                    jurisdictions: jurisdictions
                        .iter()
                        .map(|j| j.as_str().to_string())
                        .collect(),
                }],
            )
            .await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.breach.record".to_string(),
                incident.id.to_string(),
                serde_json::to_value(&incident).unwrap_or_default(),
            )
            .await?;

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(RecordBreachIncidentOutput {
            incident_id: incident.id,
            obligations,
        })
    }
}

pub struct ListIncidents {
    tx_manager: TxManager,
    breaches: Arc<dyn BreachRepository>,
}

impl ListIncidents {
    pub fn new(tx_manager: TxManager, breaches: Arc<dyn BreachRepository>) -> Self {
        Self {
            tx_manager,
            breaches,
        }
    }

    #[tracing::instrument(name = "privacy.list_incidents", skip_all)]
    pub async fn execute(&self) -> Result<Vec<BreachIncident>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_breach_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let incidents = self.breaches.list(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(incidents)
    }
}

pub struct ListObligations {
    tx_manager: TxManager,
    breaches: Arc<dyn BreachRepository>,
}

impl ListObligations {
    pub fn new(tx_manager: TxManager, breaches: Arc<dyn BreachRepository>) -> Self {
        Self {
            tx_manager,
            breaches,
        }
    }

    #[tracing::instrument(name = "privacy.list_obligations", skip_all)]
    pub async fn execute(
        &self,
        incident_id: BreachIncidentId,
    ) -> Result<Vec<NotificationObligation>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_obligation_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let obligations = self
            .breaches
            .list_obligations(&mut scope, incident_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(obligations)
    }
}

// ---------------------------------------------------------------------------
// Art. 30 records + risk assessments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpsertActivityCommand {
    pub name: String,
    pub purpose: String,
    pub data_classes: Vec<String>,
    pub controller_role: String,
    pub basis: Option<String>,
}

pub struct UpsertActivity {
    tx_manager: TxManager,
    activities: Arc<dyn ProcessingActivityRepository>,
    audit: Arc<dyn AuditWriter>,
    ids: Arc<dyn IdGenerator>,
}

impl UpsertActivity {
    pub fn new(
        tx_manager: TxManager,
        activities: Arc<dyn ProcessingActivityRepository>,
        audit: Arc<dyn AuditWriter>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            tx_manager,
            activities,
            audit,
            ids,
        }
    }

    #[tracing::instrument(name = "privacy.upsert_activity", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        command: UpsertActivityCommand,
    ) -> Result<ProcessingActivityId, PrivacyError> {
        let activity = ProcessingActivity {
            id: self.ids.next_processing_activity_id()?,
            name: command.name.clone(),
            purpose: command.purpose,
            data_classes: command
                .data_classes
                .iter()
                .map(|c| DataClass::parse(c))
                .collect::<Result<_, _>>()?,
            controller_role: crate::domain::ControllerRole::parse(&command.controller_role)?,
            basis: command.basis,
            version: 1,
            supersedes: None,
            active: true,
            created_by: Some(actor),
            created_at: Utc::now(),
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_activity_upsert", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.activities.save(&mut scope, &activity).await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.activity.upsert".to_string(),
                activity.id.to_string(),
                serde_json::to_value(&activity).unwrap_or_default(),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(activity.id)
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpsertAssessmentCommand {
    #[schema(value_type = String)]
    pub activity_id: ProcessingActivityId,
    pub jurisdictions: Vec<String>,
    pub trigger: String,
    pub status: String,
    pub findings: Option<serde_json::Value>,
    pub mitigations: Option<serde_json::Value>,
    pub next_review_at: Option<DateTime<Utc>>,
}

pub struct UpsertAssessment {
    tx_manager: TxManager,
    assessments: Arc<dyn RiskAssessmentRepository>,
    audit: Arc<dyn AuditWriter>,
    ids: Arc<dyn IdGenerator>,
}

impl UpsertAssessment {
    pub fn new(
        tx_manager: TxManager,
        assessments: Arc<dyn RiskAssessmentRepository>,
        audit: Arc<dyn AuditWriter>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            tx_manager,
            assessments,
            audit,
            ids,
        }
    }

    #[tracing::instrument(name = "privacy.upsert_assessment", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        command: UpsertAssessmentCommand,
    ) -> Result<RiskAssessmentId, PrivacyError> {
        let assessment = RiskAssessment {
            id: self.ids.next_risk_assessment_id()?,
            activity_id: command.activity_id,
            jurisdictions: command
                .jurisdictions
                .iter()
                .map(|j| crate::domain::JurisdictionRef::parse(j))
                .collect::<Result<_, _>>()?,
            trigger: crate::domain::RiskAssessmentTrigger::parse(&command.trigger)?,
            status: crate::domain::RiskAssessmentStatus::parse(&command.status)?,
            findings: command.findings,
            mitigations: command.mitigations,
            reviewed_by: Some(actor),
            reviewed_at: Some(Utc::now()),
            next_review_at: command.next_review_at,
            created_at: Utc::now(),
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_assessment_upsert", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.assessments.save(&mut scope, &assessment).await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.assessment.upsert".to_string(),
                assessment.id.to_string(),
                serde_json::to_value(&assessment).unwrap_or_default(),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(assessment.id)
    }
}

/// List active processing activities (Art. 30 export).
pub struct ListActivities {
    tx_manager: TxManager,
    activities: Arc<dyn ProcessingActivityRepository>,
}

impl ListActivities {
    pub fn new(tx_manager: TxManager, activities: Arc<dyn ProcessingActivityRepository>) -> Self {
        Self {
            tx_manager,
            activities,
        }
    }

    #[tracing::instrument(name = "privacy.list_activities", skip_all)]
    pub async fn execute(&self) -> Result<Vec<ProcessingActivity>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_activity_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let activities = self.activities.list_active(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(activities)
    }
}

/// List legal holds (operator view; released included when requested).
pub struct ListHolds {
    tx_manager: TxManager,
    holds: Arc<dyn LegalHoldRepository>,
}

impl ListHolds {
    pub fn new(tx_manager: TxManager, holds: Arc<dyn LegalHoldRepository>) -> Self {
        Self { tx_manager, holds }
    }

    #[tracing::instrument(name = "privacy.list_holds", skip_all)]
    pub async fn execute(&self, include_released: bool) -> Result<Vec<LegalHold>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_hold_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let holds = self.holds.list(&mut scope, include_released).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(holds)
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MarkBreachNotifiedCommand {
    pub jurisdiction: String,
    pub audience: String,
    pub artifact: Option<serde_json::Value>,
}

/// Record that a breach obligation was discharged (per jurisdiction,
/// per audience).
pub struct MarkBreachNotified {
    tx_manager: TxManager,
    breaches: Arc<dyn BreachRepository>,
    audit: Arc<dyn AuditWriter>,
}

impl MarkBreachNotified {
    pub fn new(
        tx_manager: TxManager,
        breaches: Arc<dyn BreachRepository>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            breaches,
            audit,
        }
    }

    #[tracing::instrument(name = "privacy.mark_breach_notified", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        incident_id: BreachIncidentId,
        command: MarkBreachNotifiedCommand,
    ) -> Result<(), PrivacyError> {
        let audience = crate::domain::ObligationAudience::parse(&command.audience)?;
        let jurisdiction = crate::domain::JurisdictionRef::parse(&command.jurisdiction)?;
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_obligation_notify", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.breaches
            .mark_notified(
                &mut scope,
                incident_id,
                jurisdiction.as_str(),
                audience,
                command.artifact,
            )
            .await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.breach.notify".to_string(),
                incident_id.to_string(),
                serde_json::json!({
                    "jurisdiction": jurisdiction.as_str(),
                    "audience": audience.as_str(),
                }),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

/// List risk assessments (operator view).
pub struct ListAssessments {
    tx_manager: TxManager,
    assessments: Arc<dyn RiskAssessmentRepository>,
}

impl ListAssessments {
    pub fn new(tx_manager: TxManager, assessments: Arc<dyn RiskAssessmentRepository>) -> Self {
        Self {
            tx_manager,
            assessments,
        }
    }

    #[tracing::instrument(name = "privacy.list_assessments", skip_all)]
    pub async fn execute(&self) -> Result<Vec<RiskAssessment>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_assessment_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let assessments = self.assessments.list(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(assessments)
    }
}
