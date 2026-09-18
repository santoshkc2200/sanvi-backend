//! Data subject request handling: submission (self-service, authorized
//! agent, tenant operator), verification (signed link + OTP), status,
//! operator extensions/rejections and rectification.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{PlatformScope, TxManager};

use crate::domain::{
    AppealId, AuditWriter, AuthorizedAgent, AuthorizedAgentRepository, Clock, ConsentRepository,
    DataSubject, DirectiveRepository, DsrId, DsrKind, DsrRepository, DsrStatus, EventPublisher,
    EvidenceKind, Hasher, IdGenerator, JurisdictionRepository, Notifications,
    ParticipantRepository, PrivacyError, PrivacyEvent, ProviderRegistry, RectifyPatch, Requester,
    SignalRepository, SubjectKind, SubjectRef, SubjectRepository, VerificationChallenge,
    VerificationChallengeId, VerificationRepository, add_days, resolve,
};

/// The subject a request targets, as supplied by the requester.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SubjectInput {
    /// `user` (authenticated), `end_user` (tenant customer), `email`.
    pub kind: String,
    pub email: Option<String>,
    pub external_id: Option<String>,
    /// Jurisdiction evidence, most trusted first (country codes: `DE`,
    /// `GB`, `US-CA`, …).
    pub evidence: Vec<EvidenceInput>,
    /// Age signal for the minor rules (e.g. declared age or a guardian
    /// attestation payload).
    pub age_signal: Option<AgeSignal>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct EvidenceInput {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AgeSignal {
    /// Declared birth year; `is_minor` wins when present.
    pub birth_year: Option<i32>,
    pub is_minor: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AgentInput {
    pub agent_name: String,
    pub agent_contact: Option<serde_json::Value>,
    /// Signed permission artifact: an opaque reference plus checksum.
    pub authorization_evidence: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SubmitDsrCommand {
    pub subject: SubjectInput,
    pub kind: DsrKind,
    pub requester: Requester,
    pub agent: Option<AgentInput>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SubmitDsrOutput {
    #[schema(value_type = String)]
    pub request_id: DsrId,
    pub status: DsrStatus,
    pub due_at: DateTime<Utc>,
    pub extended_to: Option<DateTime<Utc>>,
    pub jurisdiction: String,
    /// Present when verification is required before disclosure/deletion.
    pub verification_required: bool,
    #[schema(value_type = Option<String>)]
    pub challenge_id: Option<VerificationChallengeId>,
    /// Opaque half of the verification credential. The OTP is delivered
    /// out-of-band, so neither response nor notification is sufficient alone.
    pub verification_token: Option<String>,
    /// Opt-out kinds take effect immediately and never require
    /// verification.
    pub effective_immediately: bool,
}

#[derive(Clone)]
pub struct SubmitDsr {
    tx_manager: TxManager,
    subjects: Arc<dyn SubjectRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    requests: Arc<dyn DsrRepository>,
    participants: Arc<dyn ParticipantRepository>,
    challenges: Arc<dyn VerificationRepository>,
    agents: Arc<dyn AuthorizedAgentRepository>,
    consents: Arc<dyn ConsentRepository>,
    directives: Arc<dyn DirectiveRepository>,
    signals: Arc<dyn SignalRepository>,
    events: Arc<dyn EventPublisher>,
    notifications: Arc<dyn Notifications>,
    audit: Arc<dyn AuditWriter>,
    hasher: Arc<dyn Hasher>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    registry: ProviderRegistry,
    notice_version: String,
    otp_digits: u32,
    challenge_ttl_secs: i64,
}

#[allow(clippy::too_many_arguments)]
impl SubmitDsr {
    pub fn new(
        tx_manager: TxManager,
        subjects: Arc<dyn SubjectRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        requests: Arc<dyn DsrRepository>,
        participants: Arc<dyn ParticipantRepository>,
        challenges: Arc<dyn VerificationRepository>,
        agents: Arc<dyn AuthorizedAgentRepository>,
        consents: Arc<dyn ConsentRepository>,
        directives: Arc<dyn DirectiveRepository>,
        signals: Arc<dyn SignalRepository>,
        events: Arc<dyn EventPublisher>,
        notifications: Arc<dyn Notifications>,
        audit: Arc<dyn AuditWriter>,
        hasher: Arc<dyn Hasher>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        registry: ProviderRegistry,
        notice_version: String,
        otp_digits: u32,
        challenge_ttl_secs: i64,
    ) -> Self {
        Self {
            tx_manager,
            subjects,
            jurisdictions,
            requests,
            participants,
            challenges,
            agents,
            consents,
            directives,
            signals,
            events,
            notifications,
            audit,
            hasher,
            ids,
            clock,
            registry,
            notice_version,
            otp_digits,
            challenge_ttl_secs,
        }
    }
}

pub fn subject_ref_from_input(
    input: &SubjectInput,
    user_id: Option<UserId>,
    tenant_id: Option<TenantId>,
) -> Result<SubjectRef, PrivacyError> {
    let kind = SubjectKind::parse(&input.kind)?;
    let subject_ref = match kind {
        SubjectKind::TenantUser => {
            let user_id = user_id
                .ok_or_else(|| PrivacyError::Conflict("authenticated user required".to_string()))?;
            SubjectRef::user(user_id)
        }
        SubjectKind::EndUser => {
            let tenant_id = tenant_id.ok_or_else(|| {
                PrivacyError::Conflict("tenant required for end-user requests".to_string())
            })?;
            let external_id = input.external_id.clone().ok_or_else(|| {
                PrivacyError::Conflict("external_id required for end-user subjects".to_string())
            })?;
            SubjectRef::end_user(tenant_id, &external_id)
        }
        SubjectKind::Contact => {
            let email = input.email.clone().ok_or_else(|| {
                PrivacyError::Conflict("email required for contact subjects".to_string())
            })?;
            SubjectRef::contact(&email)
        }
        SubjectKind::Device => {
            return Err(PrivacyError::Conflict(
                "device subjects cannot submit requests directly".to_string(),
            ));
        }
    };
    Ok(subject_ref)
}

/// Resolve and persist the subject's jurisdiction (stored with its evidence
/// chain; unresolved evidence falls back to the most protective profile).
pub async fn ensure_subject(
    scope: &mut PlatformScope<'_>,
    subjects: &dyn SubjectRepository,
    jurisdictions: &dyn JurisdictionRepository,
    clock: &dyn Clock,
    ids: &dyn IdGenerator,
    subject_ref: &SubjectRef,
    evidence: &[(EvidenceKind, String)],
    is_minor: bool,
) -> Result<DataSubject, PrivacyError> {
    let now = clock.now();
    if let Some(existing) = subjects
        .find_by_key(scope, subject_ref.canonical_key())
        .await?
    {
        if existing.is_minor == is_minor {
            return Ok(existing);
        }
        let mut subject = existing;
        subject.is_minor = is_minor;
        subjects.set_minor(scope, subject.id, is_minor).await?;
        return Ok(subject);
    }
    let (jurisdiction, chain) = resolve(evidence);
    let _ = jurisdictions.find(scope, jurisdiction.as_str()).await?; // validity check
    let subject = DataSubject {
        id: ids.next_subject_id()?,
        tenant_id: subject_ref.tenant_id,
        subject_ref: subject_ref.clone(),
        jurisdiction,
        jurisdiction_evidence: chain,
        is_minor,
        created_at: now,
        updated_at: now,
    };
    subjects.save(scope, &subject).await?;
    Ok(subject)
}

impl SubmitDsr {
    #[tracing::instrument(name = "privacy.submit_dsr", skip_all, fields(tenant_id = ?tenant_id))]
    pub async fn execute(
        &self,
        requester_user: Option<UserId>,
        tenant_id: Option<TenantId>,
        ip: Option<IpAddr>,
        command: SubmitDsrCommand,
    ) -> Result<SubmitDsrOutput, PrivacyError> {
        let subject_ref = subject_ref_from_input(&command.subject, requester_user, tenant_id)?;
        let evidence: Vec<(EvidenceKind, String)> = command
            .subject
            .evidence
            .iter()
            .map(|e| EvidenceKind::parse(e.kind.as_str()).map(|kind| (kind, e.value.clone())))
            .collect::<Result<_, _>>()?;
        let is_minor = command.subject.age_signal.as_ref().is_some_and(|age| {
            age.is_minor
                || age.birth_year.is_some_and(|year| {
                    year >= chrono::Datelike::year(&Utc::now().date_naive()) - 12
                })
        });

        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_submit", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        let subject = ensure_subject(
            &mut scope,
            self.subjects.as_ref(),
            self.jurisdictions.as_ref(),
            self.clock.as_ref(),
            self.ids.as_ref(),
            &subject_ref,
            &evidence,
            is_minor,
        )
        .await?;

        let profile = self
            .jurisdictions
            .find(&mut scope, subject.jurisdiction.as_str())
            .await?
            .ok_or_else(|| {
                PrivacyError::JurisdictionNotFound(subject.jurisdiction.as_str().to_string())
            })?;

        let now = self.clock.now();
        let due_at = add_days(now, profile.response_days);

        let opt_out = command.kind.is_opt_out();
        // Verification is forbidden as a gate on opt-outs (CCPA); every
        // other kind must verify before anything is disclosed or deleted.
        // Verification is required before disclosure/deletion — except for
        // opt-outs (never gated) and authenticated tenant users (their
        // session is the verification).
        let verified_at = if opt_out
            || (requester_user.is_some() && subject_ref.kind == SubjectKind::TenantUser)
            || (requester_user.is_some()
                && tenant_id.is_some()
                && command.requester == Requester::TenantOperator
                && subject_ref.kind == SubjectKind::EndUser)
        {
            Some(now)
        } else {
            None
        };
        let acknowledged_at = Some(now);
        let status = if verified_at.is_some() {
            DsrStatus::InProgress
        } else {
            DsrStatus::AwaitingVerification
        };

        let agent = match (&command.requester, &command.agent) {
            (Requester::AuthorizedAgent, Some(agent)) => Some(agent),
            (Requester::AuthorizedAgent, None) => {
                return Err(PrivacyError::AgentEvidenceMissing);
            }
            _ => None,
        };

        let request_id = self.ids.next_dsr_id()?;
        let request = crate::domain::DataSubjectRequest {
            id: request_id,
            tenant_id: subject_ref.tenant_id,
            subject_id: subject.id,
            subject_ref: subject_ref.clone(),
            kind: command.kind,
            status,
            received_at: now,
            acknowledged_at,
            verified_at,
            due_at,
            extended_to: None,
            completed_at: None,
            jurisdiction: subject.jurisdiction.clone(),
            submitted_by: command.requester,
            agent_id: None,
            requester_ip: ip,
            requester_note: command.note.clone(),
            rejection_reason: None,
            rejected_by: None,
            appeal_id: None,
            export_artifact_id: None,
        };
        self.requests.save(&mut scope, &request).await?;

        if let Some(agent) = agent {
            let agent_row = AuthorizedAgent {
                id: self.ids.next_authorized_agent_id()?,
                request_id,
                agent_name: agent.agent_name.clone(),
                agent_contact: agent.agent_contact.clone(),
                authorization_evidence: agent.authorization_evidence.clone(),
                consumer_verified_at: None,
                verified_by: None,
                created_at: now,
            };
            self.agents.save(&mut scope, &agent_row).await?;
        }

        let mut challenge_id = None;
        let mut verification_token = None;
        let mut pending_code: Option<(String, String)> = None;
        if verified_at.is_none() {
            let code = generate_otp(self.otp_digits);
            let token = Uuid::now_v7().to_string();
            let challenge = VerificationChallenge {
                id: self.ids.next_verification_challenge_id()?,
                request_id,
                method: "email_otp".to_string(),
                token_hash: self.hasher.hash(&token),
                code_hash: self.hasher.hash(&code),
                expires_at: now + Duration::seconds(self.challenge_ttl_secs),
                attempts: 0,
                consumed_at: None,
                created_at: now,
            };
            self.challenges.save(&mut scope, &challenge).await?;
            challenge_id = Some(challenge.id);
            verification_token = Some(token);
            // Held until after the commit: the notification service is an
            // outbound HTTP dependency, and a slow or failing mailer must not
            // be able to roll back a recorded request.
            pending_code = command
                .subject
                .email
                .as_ref()
                .map(|email| (email.clone(), code));
        } else {
            self.participants
                .ensure_participants(
                    &mut scope,
                    request_id,
                    &self
                        .registry
                        .iter()
                        .map(|p| p.context_name())
                        .collect::<Vec<_>>(),
                )
                .await?;
        }

        // Opt-out kinds flip directives synchronously in this transaction:
        // an opt-out must not be blocked by anything, including verification.
        if opt_out {
            crate::application::directives::flip_directives(
                &mut scope,
                self.consents.as_ref(),
                self.directives.as_ref(),
                self.signals.as_ref(),
                self.ids.as_ref(),
                &subject,
                &profile,
                &subject_ref,
                &command.kind.denied_purposes(),
                Some(&self.notice_version),
                now,
                crate::domain::SignalSource::Api,
            )
            .await?;
        }

        self.events
            .publish(
                &mut scope,
                &[PrivacyEvent::DsrReceived {
                    request_id,
                    subject: subject_ref.clone(),
                    dsr_kind: command.kind.as_str().to_string(),
                    jurisdiction: subject.jurisdiction.as_str().to_string(),
                }],
            )
            .await?;

        self.audit
            .record(
                &mut scope,
                requester_user,
                "privacy.dsr.submit".to_string(),
                request_id.as_i64().to_string(),
                serde_json::json!({
                    "kind": command.kind.as_str(),
                    "jurisdiction": subject.jurisdiction.as_str(),
                    "submitted_by": command.requester.as_str(),
                }),
            )
            .await?;

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        if let Some((email, code)) = pending_code {
            self.notifications
                .send_verification_code(&email, &code, request_id)
                .await?;
        }

        Ok(SubmitDsrOutput {
            request_id,
            status,
            due_at,
            extended_to: None,
            jurisdiction: subject.jurisdiction.as_str().to_string(),
            verification_required: challenge_id.is_some(),
            challenge_id,
            verification_token,
            effective_immediately: opt_out,
        })
    }
}

fn generate_otp(digits: u32) -> String {
    use rand::Rng;
    let mut rng = rand::rngs::OsRng;
    let max = 10u64.pow(digits) - 1;
    let code = rng.gen_range(0..=max);
    format!("{code:0width$}", width = digits as usize)
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct VerifyRequestCommand {
    #[schema(value_type = String)]
    pub request_id: DsrId,
    pub token: String,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VerifyRequestOutput {
    #[schema(value_type = String)]
    pub request_id: DsrId,
    pub status: DsrStatus,
    pub due_at: DateTime<Utc>,
}

pub struct VerifyRequest {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    challenges: Arc<dyn VerificationRepository>,
    participants: Arc<dyn ParticipantRepository>,
    hasher: Arc<dyn Hasher>,
    clock: Arc<dyn Clock>,
    registry: ProviderRegistry,
    audit: Arc<dyn AuditWriter>,
    max_attempts: i32,
}

impl VerifyRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        challenges: Arc<dyn VerificationRepository>,
        participants: Arc<dyn ParticipantRepository>,
        hasher: Arc<dyn Hasher>,
        clock: Arc<dyn Clock>,
        registry: ProviderRegistry,
        audit: Arc<dyn AuditWriter>,
        max_attempts: i32,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            challenges,
            participants,
            hasher,
            clock,
            registry,
            audit,
            max_attempts,
        }
    }

    #[tracing::instrument(name = "privacy.verify_request", skip_all)]
    pub async fn execute(
        &self,
        command: VerifyRequestCommand,
    ) -> Result<VerifyRequestOutput, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_verify", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let request_id = command.request_id;
        let request = self
            .requests
            .find_by_id(&mut scope, request_id)
            .await?
            .ok_or(PrivacyError::RequestNotFound(
                command.request_id.to_string(),
            ))?;
        let challenge = self
            .challenges
            .find_by_request(&mut scope, request_id)
            .await?
            .ok_or(PrivacyError::ChallengeNotFound)?;
        if challenge.consumed_at.is_some() {
            return Err(PrivacyError::ChallengeExhausted);
        }
        if !self.hasher.verify(&command.token, &challenge.token_hash) {
            return Err(PrivacyError::ChallengeMismatch);
        }
        let now = self.clock.now();
        if challenge.expires_at < now {
            return Err(PrivacyError::ChallengeExpired);
        }
        if challenge.attempts >= self.max_attempts {
            return Err(PrivacyError::ChallengeExhausted);
        }
        if !self.hasher.verify(&command.code, &challenge.code_hash) {
            self.challenges
                .record_attempt(&mut scope, challenge.id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            return Err(PrivacyError::ChallengeMismatch);
        }

        self.challenges.consume(&mut scope, challenge.id).await?;
        self.requests.verify(&mut scope, request_id, now).await?;
        self.participants
            .ensure_participants(
                &mut scope,
                request_id,
                &self
                    .registry
                    .iter()
                    .map(|p| p.context_name())
                    .collect::<Vec<_>>(),
            )
            .await?;

        self.audit
            .record(
                &mut scope,
                None,
                "privacy.dsr.verify".to_string(),
                request_id.as_i64().to_string(),
                serde_json::json!({ "method": challenge.method }),
            )
            .await?;

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(VerifyRequestOutput {
            request_id: command.request_id,
            status: DsrStatus::InProgress,
            due_at: request.due_at,
        })
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GetRequestStatusOutput {
    #[schema(value_type = String)]
    pub request_id: DsrId,
    pub kind: DsrKind,
    pub status: DsrStatus,
    pub received_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub due_at: DateTime<Utc>,
    pub extended_to: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub jurisdiction: String,
    pub submitted_by: Requester,
    pub rejection_reason: Option<String>,
    pub export_available: bool,
    pub appeal: Option<AppealStatusView>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AppealStatusView {
    #[schema(value_type = String)]
    pub appeal_id: AppealId,
    pub reason: String,
    pub due_at: DateTime<Utc>,
    pub outcome: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub authority_notice: Option<crate::domain::AuthorityContact>,
}

pub struct GetRequestStatus {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    appeals: Arc<dyn crate::domain::AppealRepository>,
    challenges: Arc<dyn VerificationRepository>,
    hasher: Arc<dyn Hasher>,
}

impl GetRequestStatus {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        appeals: Arc<dyn crate::domain::AppealRepository>,
        challenges: Arc<dyn VerificationRepository>,
        hasher: Arc<dyn Hasher>,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            appeals,
            challenges,
            hasher,
        }
    }

    /// `requester_user`: the authenticated subject (if any);
    /// `token`: the verification-challenge token (if any). At least one must
    /// own the request.
    #[tracing::instrument(name = "privacy.get_request_status", skip_all)]
    pub async fn execute(
        &self,
        request_id: DsrId,
        requester_user: Option<UserId>,
        token: Option<&str>,
    ) -> Result<GetRequestStatusOutput, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_status", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let id = request_id;
        let request = self
            .requests
            .find_by_id(&mut scope, id)
            .await?
            .ok_or(PrivacyError::RequestNotFound(request_id.to_string()))?;

        let owns = requester_user.is_some_and(|user| request.subject_ref == SubjectRef::user(user));
        let token_owns = if let Some(token) = token {
            if let Some(challenge) = self.challenges.find_by_request(&mut scope, id).await? {
                self.hasher.verify(token, &challenge.token_hash)
            } else {
                false
            }
        } else {
            false
        };
        if !owns && !token_owns {
            return Err(PrivacyError::RequestNotOwned);
        }

        let appeal = self.appeals.find_by_request(&mut scope, id).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(GetRequestStatusOutput {
            request_id,
            kind: request.kind,
            status: request.status,
            received_at: request.received_at,
            acknowledged_at: request.acknowledged_at,
            due_at: request.due_at,
            extended_to: request.extended_to,
            completed_at: request.completed_at,
            jurisdiction: request.jurisdiction.as_str().to_string(),
            submitted_by: request.submitted_by,
            rejection_reason: request.rejection_reason,
            export_available: request.export_artifact_id.is_some(),
            appeal: appeal.map(|a| AppealStatusView {
                appeal_id: a.id,
                reason: a.reason,
                due_at: a.due_at,
                outcome: a.outcome.map(|o| match o {
                    crate::domain::AppealOutcome::Upheld => "upheld".to_string(),
                    crate::domain::AppealOutcome::Denied => "denied".to_string(),
                }),
                decided_at: a.decided_at,
                authority_notice: a.authority_notice,
            }),
        })
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RequestOperatorView {
    #[schema(value_type = String)]
    pub request_id: DsrId,
    #[schema(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
    pub subject_key: String,
    pub kind: DsrKind,
    pub status: DsrStatus,
    pub received_at: DateTime<Utc>,
    pub due_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub jurisdiction: String,
    pub submitted_by: Requester,
    pub rejection_reason: Option<String>,
}

pub struct ListRequestsOperator {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
}

impl ListRequestsOperator {
    pub fn new(tx_manager: TxManager, requests: Arc<dyn DsrRepository>) -> Self {
        Self {
            tx_manager,
            requests,
        }
    }

    #[tracing::instrument(name = "privacy.list_requests_operator", skip_all)]
    pub async fn execute(
        &self,
        status: Option<DsrStatus>,
        limit: u32,
    ) -> Result<Vec<RequestOperatorView>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let requests = self
            .requests
            .list_operator(&mut scope, status, limit.min(200))
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(requests.into_iter().map(request_operator_view).collect())
    }

    pub async fn execute_for_tenant(
        &self,
        tenant_id: TenantId,
        status: Option<DsrStatus>,
        limit: u32,
    ) -> Result<Vec<RequestOperatorView>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_tenant_dsr_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let requests = self
            .requests
            .list_tenant(&mut scope, tenant_id, status, limit.min(200))
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(requests.into_iter().map(request_operator_view).collect())
    }
}

fn request_operator_view(request: crate::domain::DataSubjectRequest) -> RequestOperatorView {
    RequestOperatorView {
        request_id: request.id,
        tenant_id: request.tenant_id,
        subject_key: request.subject_ref.canonical_key().to_string(),
        kind: request.kind,
        status: request.status,
        received_at: request.received_at,
        due_at: request.effective_due(),
        completed_at: request.completed_at,
        jurisdiction: request.jurisdiction.as_str().to_string(),
        submitted_by: request.submitted_by,
        rejection_reason: request.rejection_reason,
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ExtendDeadlineCommand {
    #[schema(value_type = String)]
    pub request_id: DsrId,
}

pub struct ExtendDeadline {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl ExtendDeadline {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            jurisdictions,
            clock,
            audit,
        }
    }

    #[tracing::instrument(name = "privacy.extend_deadline", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        command: ExtendDeadlineCommand,
    ) -> Result<DateTime<Utc>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_extend", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let id = command.request_id;
        let mut request = self.requests.find_by_id(&mut scope, id).await?.ok_or(
            PrivacyError::RequestNotFound(command.request_id.to_string()),
        )?;
        let profile = self
            .jurisdictions
            .find(&mut scope, request.jurisdiction.as_str())
            .await?
            .ok_or_else(|| {
                PrivacyError::JurisdictionNotFound(request.jurisdiction.as_str().to_string())
            })?;
        request.extend(self.clock.now(), &profile)?;
        self.requests
            .extend(
                &mut scope,
                id,
                request.extended_to.unwrap_or(request.due_at),
            )
            .await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.dsr.extend".to_string(),
                id.as_i64().to_string(),
                serde_json::json!({ "extended_to": request.extended_to }),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(request.extended_to.unwrap_or(request.due_at))
    }
}

pub struct RejectRequest {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl RejectRequest {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            events,
            clock,
            audit,
        }
    }

    /// Reject with reason. US subjects get the appeal route and the state
    /// authority's complaint address in the response (the handler appends
    /// them from the profile).
    #[tracing::instrument(name = "privacy.reject_request", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        request_id: DsrId,
        reason: String,
    ) -> Result<(), PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_reject", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let id = request_id;
        let mut request = self
            .requests
            .find_by_id(&mut scope, id)
            .await?
            .ok_or(PrivacyError::RequestNotFound(request_id.to_string()))?;
        request.reject(reason.clone(), self.clock.now())?;
        self.requests
            .set_status(
                &mut scope,
                id,
                DsrStatus::Rejected,
                request.completed_at,
                Some(reason),
            )
            .await?;
        self.requests.set_rejected_by(&mut scope, id, actor).await?;
        self.events
            .publish(
                &mut scope,
                &[PrivacyEvent::DsrClosed {
                    request_id,
                    subject: request.subject_ref.clone(),
                    dsr_kind: request.kind.as_str().to_string(),
                    status: "rejected".to_string(),
                    jurisdiction: request.jurisdiction.as_str().to_string(),
                }],
            )
            .await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.dsr.reject".to_string(),
                request_id.to_string(),
                serde_json::json!({ "reason": request.rejection_reason }),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

pub struct PatchRequest {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    registry: ProviderRegistry,
    audit: Arc<dyn AuditWriter>,
}

impl PatchRequest {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        registry: ProviderRegistry,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            registry,
            audit,
        }
    }

    /// Apply rectification corrections to the owning providers. Only the
    /// request's subject (authenticated) may patch.
    #[tracing::instrument(name = "privacy.patch_request", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        request_id: DsrId,
        patch: RectifyPatch,
    ) -> Result<(), PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_rectify", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let id = request_id;
        let request = self
            .requests
            .find_by_id(&mut scope, id)
            .await?
            .ok_or(PrivacyError::RequestNotFound(request_id.to_string()))?;
        if request.subject_ref != SubjectRef::user(actor) {
            return Err(PrivacyError::RequestNotOwned);
        }
        if request.kind != DsrKind::Rectification {
            return Err(PrivacyError::Conflict(
                "request is not a rectification request".to_string(),
            ));
        }
        for provider in &self.registry {
            provider.rectify(&request.subject_ref, &patch).await?;
        }
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.dsr.rectify".to_string(),
                request_id.to_string(),
                serde_json::to_value(&patch).unwrap_or_default(),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

/// The tenant-operator path: the tenant acts for its own end users
/// (processor / service-provider role). Scoped by RLS, audited, and never
/// requiring a challenge (the tenant is the controller's operator).
pub struct SubmitTenantRequest {
    inner: SubmitDsr,
}

impl SubmitTenantRequest {
    pub fn new(inner: SubmitDsr) -> Self {
        Self { inner }
    }

    #[tracing::instrument(name = "privacy.submit_tenant_request", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        operator: UserId,
        tenant_id: TenantId,
        ip: Option<IpAddr>,
        command: SubmitDsrCommand,
    ) -> Result<SubmitDsrOutput, PrivacyError> {
        let mut command = command;
        command.requester = Requester::TenantOperator;
        if command.subject.kind != "end_user" {
            return Err(PrivacyError::Conflict(
                "tenant operators may only submit for their end users".to_string(),
            ));
        }
        let subject_input = command.subject.clone();
        let subject_ref = subject_ref_from_input(&subject_input, None, Some(tenant_id))?;
        if subject_ref.tenant_id != Some(tenant_id) {
            return Err(PrivacyError::Conflict(
                "request targets a subject outside the tenant".to_string(),
            ));
        }
        self.inner
            .execute(Some(operator), Some(tenant_id), ip, command)
            .await
    }
}

// Re-export for the module tree.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otp_has_requested_digits() {
        let code = generate_otp(6);
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn subject_refs_from_inputs() {
        let user = UserId::try_from(101_i64).unwrap();
        let input = SubjectInput {
            kind: "tenant_user".to_string(),
            email: None,
            external_id: None,
            evidence: vec![],
            age_signal: None,
        };
        // TenantUser requires the authenticated user id.
        assert!(subject_ref_from_input(&input, Some(user), None).is_ok());

        let end_user = SubjectInput {
            kind: "end_user".to_string(),
            email: None,
            external_id: Some("cust-1".to_string()),
            evidence: vec![],
            age_signal: None,
        };
        let tenant = TenantId::try_from(102_i64).unwrap();
        let reference = subject_ref_from_input(&end_user, None, Some(tenant)).unwrap();
        assert_eq!(
            reference.canonical_key(),
            format!("end_user:{tenant}:cust-1")
        );
    }
}
