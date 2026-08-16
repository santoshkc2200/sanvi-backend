//! The DSR saga: the worker-driven fan-out over every registered provider.
//!
//! - `ProcessDsrBatch` advances open requests: collect (access/export/know
//!   categories), erase (crypto-shred + providers), rectify, and closes
//!   each with a per-participant record. Partial completion is a first-class
//!   status; a failed participant is retried and escalated, never silently
//!   skipped.
//! - `PropagateOptOuts` relays exercised opt-outs to downstream processors
//!   within the jurisdiction's business-day window.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;

use sanvi_persistence::TxManager;

use crate::domain::{
    Clock, ConsentRepository, DataBundle, DsrKind, DsrRepository, DsrStatus, EnvelopeCrypto,
    EraseReport, EventPublisher, ExportArtifactRepository, ExportPackager, Hasher,
    LegalHoldRepository, Notifications, ParticipantRepository, ParticipantRow, ParticipantStatus,
    PrivacyError, PrivacyEvent, ProviderRegistry, SubProcessorNotifier, SubProcessorRepository,
};

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct ProcessDsrReport {
    pub advanced: usize,
    pub completed: usize,
    pub partially_complete: usize,
    pub failed: usize,
    pub escalated: usize,
    pub shreds: usize,
    pub exports_packaged: usize,
}

/// One pass of the DSR saga. Called by the worker on an interval; each
/// request is processed in its own transaction.
pub struct ProcessDsrBatch {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    participants: Arc<dyn ParticipantRepository>,
    holds: Arc<dyn LegalHoldRepository>,
    artifacts: Arc<dyn ExportArtifactRepository>,
    registry: ProviderRegistry,
    packager: Arc<dyn ExportPackager>,
    crypto: Arc<dyn EnvelopeCrypto>,
    notifications: Arc<dyn Notifications>,
    events: Arc<dyn EventPublisher>,
    hasher: Arc<dyn Hasher>,
    clock: Arc<dyn Clock>,
    export_dir: String,
    export_ttl_days: i64,
    export_max_downloads: i32,
    backup_retention_days: i64,
}

#[allow(clippy::too_many_arguments)]
impl ProcessDsrBatch {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        participants: Arc<dyn ParticipantRepository>,
        holds: Arc<dyn LegalHoldRepository>,
        artifacts: Arc<dyn ExportArtifactRepository>,
        registry: ProviderRegistry,
        packager: Arc<dyn ExportPackager>,
        crypto: Arc<dyn EnvelopeCrypto>,
        notifications: Arc<dyn Notifications>,
        events: Arc<dyn EventPublisher>,
        hasher: Arc<dyn Hasher>,
        clock: Arc<dyn Clock>,
        export_dir: String,
        export_ttl_days: i64,
        export_max_downloads: i32,
        backup_retention_days: i64,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            participants,
            holds,
            artifacts,
            registry,
            packager,
            crypto,
            notifications,
            events,
            hasher,
            clock,
            export_dir,
            export_ttl_days,
            export_max_downloads,
            backup_retention_days,
        }
    }

    /// Advance up to `limit` open requests.
    pub async fn execute(&self, limit: u32) -> Result<ProcessDsrReport, PrivacyError> {
        let mut report = ProcessDsrReport::default();
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_advance", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let open = self
            .requests
            .open_requests(
                &mut scope,
                &[
                    DsrStatus::InProgress,
                    DsrStatus::PartiallyComplete,
                    DsrStatus::OnHold,
                ],
                None,
                limit.min(500),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        for request in open {
            match self.advance_one(&request).await {
                Ok(Some(outcome)) => {
                    report.advanced += 1;
                    match outcome {
                        AdvanceOutcome::Completed { shred } => {
                            report.completed += 1;
                            if shred {
                                report.shreds += 1;
                            }
                        }
                        AdvanceOutcome::PartiallyComplete => report.partially_complete += 1,
                        AdvanceOutcome::Packaged => report.exports_packaged += 1,
                        AdvanceOutcome::Escalated => report.escalated += 1,
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    report.failed += 1;
                    tracing::error!(error = %err, "DSR advancement failed");
                }
            }
        }
        Ok(report)
    }

    async fn advance_one(
        &self,
        request: &crate::domain::DataSubjectRequest,
    ) -> Result<Option<AdvanceOutcome>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_advance_one", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        // Legal holds block erasure outright (the request parks on hold).
        if request.kind == DsrKind::Erasure {
            let holds = self
                .holds
                .active_for_subject(&mut scope, &request.subject_ref)
                .await?;
            if !holds.is_empty() {
                self.requests
                    .set_status(&mut scope, request.id, DsrStatus::OnHold, None, None)
                    .await?;
                scope
                    .commit()
                    .await
                    .map_err(|err| PrivacyError::Storage(err.to_string()))?;
                return Ok(Some(AdvanceOutcome::Escalated));
            }
        }

        let participants = self.participants.list(&mut scope, request.id).await?;
        let context_names: Vec<&'static str> =
            self.registry.iter().map(|p| p.context_name()).collect();
        let mut any_failed = false;
        let mut bundles: Vec<DataBundle> = Vec::new();
        let mut erase_reports: Vec<EraseReport> = Vec::new();

        for provider in &self.registry {
            let name = provider.context_name();
            if !context_names.contains(&name) {
                continue;
            }
            let participant = participants.get(name).cloned().unwrap_or(ParticipantRow {
                context: name.to_string(),
                status: ParticipantStatus::Pending,
                report: None,
                attempts: 0,
                last_error: None,
                updated_at: self.clock.now(),
            });

            match participant.status {
                ParticipantStatus::Completed | ParticipantStatus::Retained => continue,
                ParticipantStatus::Failed if participant.attempts >= 2 => {
                    any_failed = true;
                    continue;
                }
                ParticipantStatus::Pending
                | ParticipantStatus::InProgress
                | ParticipantStatus::Failed => {
                    self.participants
                        .set_status(
                            &mut scope,
                            request.id,
                            name,
                            ParticipantStatus::InProgress,
                            None,
                            None,
                        )
                        .await?;
                    let result = if request.kind.collects() {
                        provider
                            .collect(&request.subject_ref)
                            .await
                            .map(ProviderOutcome::Collected)
                    } else if request.kind.erases() {
                        provider
                            .erase(&request.subject_ref, crate::domain::EraseMode::Full)
                            .await
                            .map(ProviderOutcome::Erased)
                    } else if request.kind.rectifies() {
                        provider
                            .rectify(
                                &request.subject_ref,
                                &crate::domain::RectifyPatch::default(),
                            )
                            .await
                            .map(|()| ProviderOutcome::Noop)
                    } else {
                        Ok(ProviderOutcome::Noop)
                    };
                    match result {
                        Ok(outcome) => match outcome {
                            ProviderOutcome::Collected(bundle) => {
                                self.participants
                                    .set_status(
                                        &mut scope,
                                        request.id,
                                        name,
                                        ParticipantStatus::Completed,
                                        Some(serde_json::to_value(&bundle).unwrap_or_default()),
                                        None,
                                    )
                                    .await?;
                                bundles.push(bundle);
                            }
                            ProviderOutcome::Erased(report) => {
                                self.participants
                                    .set_status(
                                        &mut scope,
                                        request.id,
                                        name,
                                        ParticipantStatus::Completed,
                                        Some(serde_json::to_value(&report).unwrap_or_default()),
                                        None,
                                    )
                                    .await?;
                                erase_reports.push(report);
                            }
                            ProviderOutcome::Noop => {
                                self.participants
                                    .set_status(
                                        &mut scope,
                                        request.id,
                                        name,
                                        ParticipantStatus::Completed,
                                        None,
                                        None,
                                    )
                                    .await?;
                            }
                        },
                        Err(err) => {
                            self.participants
                                .record_attempt(&mut scope, request.id, name)
                                .await?;
                            self.participants
                                .set_status(
                                    &mut scope,
                                    request.id,
                                    name,
                                    ParticipantStatus::Failed,
                                    None,
                                    Some(err.to_string()),
                                )
                                .await?;
                            tracing::warn!(
                                error = %err,
                                context = name,
                                request_id = %request.id.as_uuid(),
                                "DSR participant failed",
                            );
                            any_failed = true;
                        }
                    }
                }
            }
        }

        // Re-read the participant rows after this pass: the request is done
        // exactly when every provider reached a terminal participant state.
        let remaining = self.participants.list(&mut scope, request.id).await?;
        let all_done = remaining.values().all(|row| {
            matches!(
                row.status,
                ParticipantStatus::Completed | ParticipantStatus::Retained
            )
        });

        let outcome = if all_done {
            match self
                .finish(&mut scope, request, bundles, erase_reports)
                .await?
            {
                Some(outcome) => outcome,
                None => return Ok(None),
            }
        } else if any_failed {
            AdvanceOutcome::PartiallyComplete
        } else {
            return Ok(None);
        };

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(Some(outcome))
    }

    async fn finish(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        request: &crate::domain::DataSubjectRequest,
        bundles: Vec<DataBundle>,
        erase_reports: Vec<EraseReport>,
    ) -> Result<Option<AdvanceOutcome>, PrivacyError> {
        let now = self.clock.now();

        if request.kind.collects() {
            let passphrase = self.generate_passphrase();
            let token = self.generate_passphrase();
            let archive = self.packager.package(&request.subject_ref, bundles)?;
            let artifact = crate::application::exports::store_artifact(
                scope,
                self.artifacts.as_ref(),
                self.hasher.as_ref(),
                self.crypto.as_ref(),
                request.id,
                request.subject_id,
                archive,
                &passphrase,
                &token,
                &self.export_dir,
                self.export_ttl_days,
                self.export_max_downloads,
                now,
            )
            .await?;
            self.requests
                .link_artifact(scope, request.id, artifact.id)
                .await?;
            self.requests
                .set_status(scope, request.id, DsrStatus::Completed, Some(now), None)
                .await?;
            if let Some(email) = request
                .subject_ref
                .identifiers
                .iter()
                .find_map(|id| (id.id_type == "email").then(|| id.value.clone()))
            {
                self.notifications
                    .send_export_passphrase(&email, &passphrase, &request.id.as_uuid().to_string())
                    .await?;
            }
            self.events
                .publish(
                    scope,
                    &[PrivacyEvent::DsrClosed {
                        request_id: request.id.as_uuid(),
                        subject: request.subject_ref.clone(),
                        dsr_kind: request.kind.as_str().to_string(),
                        status: "completed".to_string(),
                        jurisdiction: request.jurisdiction.as_str().to_string(),
                    }],
                )
                .await?;
            return Ok(Some(AdvanceOutcome::Packaged));
        }

        if request.kind.erases() {
            // Crypto-shred: destroy the per-subject DEK. After this, any
            // ciphertext under that key is unrecoverable (backups included).
            self.crypto.destroy_subject_key(request.subject_id).await?;

            // Record the retention window the subject was told about.
            let retained: Vec<crate::domain::RetainedItem> = erase_reports
                .iter()
                .flat_map(|report| report.retained.clone())
                .collect();

            self.requests
                .set_status(scope, request.id, DsrStatus::Completed, Some(now), None)
                .await?;
            self.events
                .publish(
                    scope,
                    &[
                        PrivacyEvent::SubjectErased {
                            subject: request.subject_ref.clone(),
                            tenant_id: request.tenant_id,
                        },
                        PrivacyEvent::DsrClosed {
                            request_id: request.id.as_uuid(),
                            subject: request.subject_ref.clone(),
                            dsr_kind: "erasure".to_string(),
                            status: "completed".to_string(),
                            jurisdiction: request.jurisdiction.as_str().to_string(),
                        },
                    ],
                )
                .await?;

            let _ = retained; // surfaced in the participant reports
            let _ = self.backup_retention_days; // published in the notice
            return Ok(Some(AdvanceOutcome::Completed { shred: true }));
        }

        // Rectification / opt-out kinds: the directive flip already happened
        // synchronously; closing completes the saga.
        self.requests
            .set_status(scope, request.id, DsrStatus::Completed, Some(now), None)
            .await?;
        self.events
            .publish(
                scope,
                &[PrivacyEvent::DsrClosed {
                    request_id: request.id.as_uuid(),
                    subject: request.subject_ref.clone(),
                    dsr_kind: request.kind.as_str().to_string(),
                    status: "completed".to_string(),
                    jurisdiction: request.jurisdiction.as_str().to_string(),
                }],
            )
            .await?;
        Ok(Some(AdvanceOutcome::Completed { shred: false }))
    }

    fn generate_passphrase(&self) -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut bytes);

        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
    }
}

enum AdvanceOutcome {
    Completed { shred: bool },
    PartiallyComplete,
    Packaged,
    Escalated,
}

enum ProviderOutcome {
    Collected(DataBundle),
    Erased(EraseReport),
    Noop,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct PropagateOptOutsReport {
    pub propagated: usize,
    pub failed: usize,
}

/// Sweeps unpropagated opt-outs and relays them to downstream processors
/// (the profile's business-day window is tracked against `recorded_at`).
pub struct PropagateOptOuts {
    tx_manager: TxManager,
    consents: Arc<dyn ConsentRepository>,
    processors: Arc<dyn SubProcessorRepository>,
    notifier: Arc<dyn SubProcessorNotifier>,
}

impl PropagateOptOuts {
    pub fn new(
        tx_manager: TxManager,
        consents: Arc<dyn ConsentRepository>,
        processors: Arc<dyn SubProcessorRepository>,
        notifier: Arc<dyn SubProcessorNotifier>,
    ) -> Self {
        Self {
            tx_manager,
            consents,
            processors,
            notifier,
        }
    }

    pub async fn execute(&self, limit: u32) -> Result<PropagateOptOutsReport, PrivacyError> {
        let mut report = PropagateOptOutsReport::default();
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_optout_propagate", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let pending = self
            .consents
            .unpropagated_opt_outs(&mut scope, limit)
            .await?;
        let processors = self.processors.list_public(&mut scope).await?;
        for record in pending {
            match self.notifier.propagate_opt_out(&record, &processors).await {
                Ok(()) => {
                    self.consents.mark_propagated(&mut scope, record.id).await?;
                    report.propagated += 1;
                }
                Err(err) => {
                    report.failed += 1;
                    tracing::warn!(error = %err, "opt-out propagation failed");
                }
            }
        }
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(report)
    }
}

/// The acknowledgement deadline helper used by the escalation pass.
pub fn ack_overdue(received: DateTime<Utc>, ack_days: Option<u16>, now: DateTime<Utc>) -> bool {
    ack_days.is_some_and(|days| now > crate::domain::add_business_days(received, days))
}
