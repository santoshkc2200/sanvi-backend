//! TASK-029 (11.10): the compliance drill harness — the machinery the
//! `dsr-check` CLI and the `compliance-tests` crate share, so the drill the
//! operator runs and the drill CI runs are the *same* drill.
//!
//! Every drill runs the real end-to-end path over the real provider
//! registry (all fifteen contexts) against a real Postgres: submit →
//! (saga passes) → terminal status, timed against the *statutory* clock
//! the live jurisdiction profile carries — never against the internal SLA.
//! A path that cannot complete is a failed drill, not a slow one.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use sanvi_kernel::{NodeId, RawIdGenerator, SnowflakeGenerator, UserId};
use sanvi_persistence::TxManager;
use sanvi_privacy::PrivacyConfig;
use sanvi_privacy::application::ProcessDsrBatch;
use sanvi_privacy::domain::{
    DirectiveRepository, DirectiveSnapshot, DirectiveState, DsrId, DsrKind, DsrRepository,
    DsrStatus, JurisdictionProfile, JurisdictionRepository, ParticipantRepository,
    ProcessingPurpose, SubjectKind, SubjectRef, SubjectRepository,
};
use sanvi_telemetry::Redacted;

use crate::{PrivacyProviderDeps, PrivacyWiring, build_privacy_state, privacy_provider_registry};

/// The four DSR paths the drills time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrillKind {
    Access,
    Erasure,
    OptOut,
    Appeal,
}

impl DrillKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Erasure => "erasure",
            Self::OptOut => "opt_out",
            Self::Appeal => "appeal",
        }
    }
}

/// One drill's measured result, as `dsr-check` commits it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DrillTiming {
    pub jurisdiction: String,
    pub drill: String,
    /// Wall-clock the drill's pipeline took, in milliseconds.
    pub measured_ms: i64,
    /// The statutory deadline, in milliseconds (0 = "immediate").
    pub deadline_ms: i64,
    /// Human-readable statement of the clock the deadline comes from.
    pub deadline_source: String,
    pub within_deadline: bool,
    pub detail: serde_json::Value,
}

/// A drill subject: a fresh platform user whose jurisdiction evidence
/// resolves to the drill's profile.
pub struct DrillSubject {
    pub user_id: UserId,
    pub subject_ref: SubjectRef,
}

pub struct ComplianceHarness {
    ids_snowflakes: SnowflakeGenerator,
    pub pool: sanvi_persistence::PgPool,
    pub wiring: PrivacyWiring,
    pub tx: TxManager,
    pub process_dsr: Arc<ProcessDsrBatch>,
    pub requests: Arc<dyn DsrRepository>,
    pub participants: Arc<dyn ParticipantRepository>,
    pub subjects: Arc<dyn SubjectRepository>,
    pub directives: Arc<dyn DirectiveRepository>,
    pub jurisdictions: Arc<dyn JurisdictionRepository>,
    /// Export artifacts land here during the access drill.
    pub export_dir: std::path::PathBuf,
}

impl ComplianceHarness {
    /// Build the harness over `pool`. The external systems (Kratos, Keto,
    /// Stripe) are pointed at localhost: a drill subject only has rows in
    /// the privacy ledger, so no provider ever dials them — but the paths
    /// are the real ones.
    pub fn build(pool: sanvi_persistence::PgPool, ids: Arc<dyn RawIdGenerator>) -> Self {
        let export_dir =
            std::env::temp_dir().join(format!("sanvi-compliance-drill-{}", uuid::Uuid::now_v7()));
        let config = PrivacyConfig {
            kek: Redacted::new(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                [7u8; 32],
            )),
            export_dir: export_dir.to_string_lossy().to_string(),
            ..PrivacyConfig::default()
        };
        let tx = TxManager::new(pool.clone(), ids.clone());
        let audit: Arc<dyn sanvi_audit::domain::AuditWriter> =
            Arc::new(sanvi_audit::adapters::SqlxAuditWriter::new(ids.clone()));
        let registry = privacy_provider_registry(&PrivacyProviderDeps {
            media: sanvi_media::MediaConfig::default(),
            product_access: sanvi_product_access::ProductAccessConfig::default(),
            tx: tx.clone(),
            kratos_public_url: "http://127.0.0.1:4433".to_string(),
            kratos_admin_url: "http://127.0.0.1:4434".to_string(),
            keto_read_url: "http://127.0.0.1:4466".to_string(),
            keto_write_url: "http://127.0.0.1:4467".to_string(),
            session_cookie_name: "sanvi_session_drill".to_string(),
            stripe_api_base: "http://127.0.0.1:12111".to_string(),
            stripe_api_key: Redacted::new("sk_test_drill".to_string()),
            stripe_api_version: "2024-06-20".to_string(),
            audit_writer: audit.clone(),
            privacy_lock: Arc::new(sanvi_platform_admin::adapters::NoopPrivacyLock),
        });
        let wiring = build_privacy_state(pool.clone(), ids, &config, registry, audit, None, None);
        let process_dsr = wiring.process_dsr.clone();
        Self {
            ids_snowflakes: SnowflakeGenerator::new(NodeId::new(77).expect("node id in range")),
            pool,
            process_dsr,
            wiring,
            requests: Arc::new(sanvi_privacy::adapters::SqlxDsrRepository::new()),
            participants: Arc::new(sanvi_privacy::adapters::SqlxParticipantRepository::new()),
            subjects: Arc::new(sanvi_privacy::adapters::SqlxSubjectRepository::new()),
            directives: Arc::new(sanvi_privacy::adapters::SqlxDirectiveRepository::new()),
            jurisdictions: Arc::new(sanvi_privacy::adapters::SqlxJurisdictionRepository::new()),
            tx,
            export_dir,
        }
    }

    fn next_user_id(&self) -> UserId {
        UserId::from_snowflake(self.ids_snowflakes.next_id().expect("snowflake"))
    }

    /// Every jurisdiction profile from the live configuration (the
    /// database), ordered by code — the drills run per profile, so a new
    /// profile is drilled without a code change.
    pub async fn profiles(&self) -> anyhow::Result<Vec<JurisdictionProfile>> {
        let mut scope = self
            .tx
            .begin_unscoped("compliance_profiles", "cli:dsr-check")
            .await?;
        let profiles = self.jurisdictions.list(&mut scope).await?;
        scope.commit().await?;
        let mut profiles = profiles;
        profiles.sort_by(|a, b| a.code.as_str().cmp(b.code.as_str()));
        Ok(profiles)
    }

    pub async fn profile(&self, code: &str) -> anyhow::Result<JurisdictionProfile> {
        self.profiles()
            .await?
            .into_iter()
            .find(|p| p.code.as_str() == code)
            .ok_or_else(|| {
                anyhow::anyhow!("no jurisdiction profile `{code}` in live configuration")
            })
    }

    /// A fresh drill subject: a platform user, so submission counts as
    /// verified (the session is the verification) and the drill exercises
    /// the full saga, not the OTP gate.
    pub fn drill_subject(&self) -> DrillSubject {
        let user_id = self.next_user_id();
        DrillSubject {
            user_id,
            subject_ref: SubjectRef {
                kind: SubjectKind::TenantUser,
                tenant_id: None,
                key: format!("user:{user_id}"),
                identifiers: vec![sanvi_privacy::domain::SubjectIdentifier {
                    id_type: "user_id".to_string(),
                    value: user_id.to_string(),
                }],
            },
        }
    }

    /// Drive the saga until the request is terminal. A request still open
    /// after `max_passes` passes is a stuck path — the drill fails on it
    /// rather than quietly outliving the deadline.
    pub async fn drive_to_completion(
        &self,
        request_id: DsrId,
        max_passes: u32,
    ) -> anyhow::Result<DsrStatus> {
        for _ in 0..max_passes {
            self.process_dsr.execute(50).await?;
            let status = self.request_status(request_id).await?;
            if status.is_terminal() {
                return Ok(status);
            }
        }
        anyhow::bail!(
            "drill request {request_id} did not reach a terminal status in {max_passes} saga passes"
        )
    }

    pub async fn request_status(&self, request_id: DsrId) -> anyhow::Result<DsrStatus> {
        let mut scope = self
            .tx
            .begin_unscoped("compliance_request_status", "cli:dsr-check")
            .await?;
        let request = self
            .requests
            .find_by_id(&mut scope, request_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("drill request {request_id} vanished"))?;
        scope.commit().await?;
        Ok(request.status)
    }

    async fn completed_at(&self, request_id: DsrId) -> anyhow::Result<DateTime<Utc>> {
        let mut scope = self
            .tx
            .begin_unscoped("compliance_completed_at", "cli:dsr-check")
            .await?;
        let request = self
            .requests
            .find_by_id(&mut scope, request_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("request vanished"))?;
        scope.commit().await?;
        request
            .completed_at
            .ok_or_else(|| anyhow::anyhow!("terminal request without completed_at"))
    }

    /// The directive snapshot for a subject (the drills assert the
    /// opt-out/GPC state on it).
    pub async fn directive_snapshot(
        &self,
        subject: &SubjectRef,
    ) -> anyhow::Result<DirectiveSnapshot> {
        self.wiring
            .resolver
            .snapshot(subject)
            .await
            .map_err(|err| anyhow::anyhow!("directive resolver: {err}"))
    }

    /// The participant rows of a request — the saga's per-context evidence.
    pub async fn participants_of(
        &self,
        request_id: DsrId,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let mut scope = self
            .tx
            .begin_unscoped("compliance_participants", "cli:dsr-check")
            .await?;
        let rows = self.participants.list(&mut scope, request_id).await?;
        scope.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(context, row)| (context, row.status.as_str().to_string()))
            .collect())
    }

    fn submit_command(
        &self,
        code: &str,
        kind: DsrKind,
    ) -> sanvi_privacy::application::SubmitDsrCommand {
        sanvi_privacy::application::SubmitDsrCommand {
            subject: sanvi_privacy::application::SubjectInput {
                kind: "tenant_user".to_string(),
                email: None,
                external_id: None,
                evidence: vec![sanvi_privacy::application::EvidenceInput {
                    kind: "declared_residency".to_string(),
                    value: representative_country(code).to_string(),
                }],
                age_signal: None,
            },
            kind,
            requester: sanvi_privacy::domain::Requester::Subject,
            agent: None,
            note: Some("TASK-029 compliance drill".to_string()),
        }
    }

    async fn submit_and_complete(
        &self,
        code: &str,
        kind: DsrKind,
    ) -> anyhow::Result<(
        sanvi_privacy::application::SubmitDsrOutput,
        DsrStatus,
        DateTime<Utc>,
    )> {
        let subject = self.drill_subject();
        let submitted = self
            .wiring
            .state
            .submit_dsr
            .execute(
                Some(subject.user_id),
                None,
                None,
                self.submit_command(code, kind),
            )
            .await
            .map_err(|err| anyhow::anyhow!("submit {kind:?}: {err}"))?;
        if submitted.verification_required {
            anyhow::bail!("drill subject unexpectedly required verification");
        }
        let status = self.drive_to_completion(submitted.request_id, 10).await?;
        let completed_at = self.completed_at(submitted.request_id).await?;
        Ok((submitted, status, completed_at))
    }

    fn assert_all_contexts_participated(
        &self,
        participants: &[(String, String)],
        drill: &str,
    ) -> anyhow::Result<()> {
        let missing: Vec<&str> = sanvi_privacy::domain::CONTEXT_NAMES
            .iter()
            .filter(|c| {
                !participants
                    .iter()
                    .any(|(p, s)| p == *c && s == "completed")
            })
            .copied()
            .collect();
        if !missing.is_empty() {
            anyhow::bail!("{drill} drill: contexts not completed: {missing:?}");
        }
        Ok(())
    }
}

/// Run one DSR drill for one jurisdiction, timing it against that
/// jurisdiction's statutory clock from the live profile.
pub async fn run_dsr_drill(
    harness: &ComplianceHarness,
    code: &str,
    kind: DrillKind,
) -> anyhow::Result<DrillTiming> {
    let profile = harness.profile(code).await?;
    let started = Instant::now();

    let timing = match kind {
        DrillKind::Access => {
            let (submitted, status, completed_at) =
                harness.submit_and_complete(code, DsrKind::Access).await?;
            if status != DsrStatus::Completed {
                anyhow::bail!("access drill ended {status:?}, expected completed");
            }
            let participants = harness.participants_of(submitted.request_id).await?;
            harness.assert_all_contexts_participated(&participants, "access")?;
            timing(
                code,
                kind,
                started,
                i64::from(profile.response_days) * 86_400_000,
                format!(
                    "{} response window ({} days, extendable {})",
                    code, profile.response_days, profile.extension_days
                ),
                serde_json::json!({
                    "request_id": submitted.request_id.to_string(),
                    "completed_at": completed_at.to_rfc3339(),
                    "due_at": submitted.due_at.to_rfc3339(),
                    "participants": participants.len(),
                }),
            )
        }
        DrillKind::Erasure => {
            let (submitted, status, completed_at) =
                harness.submit_and_complete(code, DsrKind::Erasure).await?;
            if status != DsrStatus::Completed {
                anyhow::bail!("erasure drill ended {status:?}, expected completed");
            }
            let participants = harness.participants_of(submitted.request_id).await?;
            harness.assert_all_contexts_participated(&participants, "erasure")?;
            timing(
                code,
                kind,
                started,
                i64::from(profile.response_days) * 86_400_000,
                format!("{} response window ({} days)", code, profile.response_days),
                serde_json::json!({
                    "request_id": submitted.request_id.to_string(),
                    "completed_at": completed_at.to_rfc3339(),
                    "due_at": submitted.due_at.to_rfc3339(),
                    "participants": participants.len(),
                }),
            )
        }
        DrillKind::OptOut => {
            let subject = harness.drill_subject();
            let command = sanvi_privacy::application::RecordOptOutCommand {
                purposes: vec![
                    ProcessingPurpose::SaleOrShare,
                    ProcessingPurpose::TargetedAdvertising,
                ],
                source: "gpc".to_string(),
                device_ref: None,
            };
            let output = harness
                .wiring
                .state
                .record_opt_out
                .execute(&subject.subject_ref, command)
                .await
                .map_err(|err| anyhow::anyhow!("opt-out: {err}"))?;
            // The US-state statutory clock is the business-day
            // effectiveness window; everywhere else the opt-out is
            // immediate. Either way the directives must really deny — an
            // opt-out that never lands has no deadline to miss.
            let snapshot = harness.directive_snapshot(&subject.subject_ref).await?;
            let denied = snapshot
                .directives
                .iter()
                .filter(|d| {
                    d.purpose == ProcessingPurpose::SaleOrShare
                        || d.purpose == ProcessingPurpose::TargetedAdvertising
                })
                .all(|d| d.state == DirectiveState::Denied);
            if !denied {
                anyhow::bail!("opt-out drill: sale/share or targeted-ads directive not denied");
            }
            let (deadline_ms, source) = match profile.opt_out_effective_business_days {
                Some(days) => (
                    i64::from(days) * 86_400_000,
                    format!("{code} opt-out effectiveness ({days} business days)"),
                ),
                None => (
                    0,
                    format!("{code}: opt-out effective immediately (no statutory window)"),
                ),
            };
            timing(
                code,
                kind,
                started,
                deadline_ms,
                source,
                serde_json::json!({
                    "jurisdiction": output.jurisdiction,
                    "directives_denied": snapshot
                        .directives
                        .iter()
                        .filter(|d| d.state == DirectiveState::Denied)
                        .count(),
                }),
            )
        }
        DrillKind::Appeal => {
            let Some(response_days) = profile.appeal_response_days else {
                return Ok(DrillTiming {
                    jurisdiction: code.to_string(),
                    drill: DrillKind::Appeal.as_str().to_string(),
                    measured_ms: 0,
                    deadline_ms: 0,
                    deadline_source: format!(
                        "{code} provides no appeal right — drill not applicable"
                    ),
                    within_deadline: true,
                    detail: serde_json::json!({ "not_applicable": true }),
                });
            };
            // Second-operator review: the deciding user must differ from
            // the rejecting one, so the drill carries two actors.
            let rejecting = harness.drill_subject().user_id;
            let deciding = harness.drill_subject().user_id;
            let submitted = harness
                .wiring
                .state
                .submit_dsr
                .execute(
                    Some(rejecting),
                    None,
                    None,
                    harness.submit_command(code, DsrKind::Access),
                )
                .await
                .map_err(|err| anyhow::anyhow!("submit: {err}"))?;
            harness
                .wiring
                .state
                .reject_request
                .execute(
                    rejecting,
                    submitted.request_id,
                    "drill: outside scope".to_string(),
                )
                .await
                .map_err(|err| anyhow::anyhow!("reject: {err}"))?;
            let appealed = harness
                .wiring
                .state
                .appeal_request
                .execute(
                    Some(rejecting),
                    sanvi_privacy::application::AppealRequestCommand {
                        request_id: submitted.request_id,
                        reason: "drill: contesting rejection".to_string(),
                    },
                )
                .await
                .map_err(|err| anyhow::anyhow!("appeal: {err}"))?;
            let decided = harness
                .wiring
                .state
                .decide_appeal
                .execute(
                    deciding,
                    sanvi_privacy::application::DecideAppealCommand {
                        appeal_id: appealed.appeal_id,
                        outcome: sanvi_privacy::domain::AppealOutcome::Upheld,
                        reason: "drill: upheld".to_string(),
                    },
                )
                .await
                .map_err(|err| anyhow::anyhow!("decide appeal: {err}"))?;
            timing(
                code,
                kind,
                started,
                i64::from(response_days) * 86_400_000,
                format!("{code} appeal decision window ({response_days} days)"),
                serde_json::json!({
                    "appeal_id": appealed.appeal_id.to_string(),
                    "outcome": format!("{:?}", decided.outcome).to_lowercase(),
                }),
            )
        }
    };
    Ok(timing)
}

fn timing(
    jurisdiction: &str,
    kind: DrillKind,
    started: Instant,
    deadline_ms: i64,
    deadline_source: String,
    detail: serde_json::Value,
) -> DrillTiming {
    let measured_ms = started.elapsed().as_millis() as i64;
    // A zero deadline is an "immediate" obligation (no statutory window):
    // the drill's own assertion already proved the effect landed, so there
    // is no clock to race — any measured latency is inside it.
    let within_deadline = deadline_ms == 0 || measured_ms <= deadline_ms;
    DrillTiming {
        jurisdiction: jurisdiction.to_string(),
        drill: kind.as_str().to_string(),
        measured_ms,
        deadline_ms,
        deadline_source,
        within_deadline,
        detail,
    }
}

/// The country code whose resolution lands on each seeded profile.
pub fn representative_country(code: &str) -> &'static str {
    match code {
        "global" => "US-OR",
        "eu" => "DE",
        "uk" => "GB",
        "jp" => "JP",
        "us-ca" => "US-CA",
        "us-va" => "US-VA",
        "us-co" => "US-CO",
        "us-tx" => "US-TX",
        "us-ut" => "US-UT",
        "us-ct" => "US-CT",
        // Unresolvable evidence → most protective, matching the resolver.
        _ => "DE",
    }
}
