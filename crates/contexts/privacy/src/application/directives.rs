//! The directive engine's application half: consent/withdrawal recording,
//! opt-out (no verification gate), "limit my sensitive PI", GPC binding and
//! the persisted `DirectiveResolver` other contexts consume.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sanvi_persistence::{PlatformScope, TxManager};

use crate::domain::{
    Clock, ConsentMethod, ConsentRecord, ConsentRepository, DataSubject, DirectiveInput,
    DirectiveRepository, DirectiveResolver, DirectiveSnapshot, DirectiveSource, DirectiveState,
    EventPublisher, EvidenceKind, IdGenerator, JurisdictionProfile, JurisdictionRepository,
    PrivacyError, PrivacyEvent, ProcessingPurpose, PurposeDirective, SignalRepository,
    SignalSource, SubjectId, SubjectRef, SubjectRepository, jurisdiction_default, resolve,
    resolve_snapshot,
};

/// Flip a set of purposes to `Denied` with the given source, ledgering the
/// change (opt-out proof) and materialising the directives. Called inside
/// the caller's transaction.
#[allow(clippy::too_many_arguments)]
pub async fn flip_directives(
    scope: &mut PlatformScope<'_>,
    consents: &dyn ConsentRepository,
    directives: &dyn DirectiveRepository,
    signals: &dyn SignalRepository,
    ids: &dyn IdGenerator,
    subject: &DataSubject,
    profile: &JurisdictionProfile,
    subject_ref: &SubjectRef,
    purposes: &[ProcessingPurpose],
    notice_version: Option<&str>,
    now: DateTime<Utc>,
    source: SignalSource,
) -> Result<(), PrivacyError> {
    let notice_version = notice_version.unwrap_or("").to_string();
    for purpose in purposes {
        // The opt-out ledger row (proof the refusal was honoured).
        consents
            .append(
                scope,
                &ConsentRecord {
                    id: ids.next_consent_id()?,
                    tenant_id: subject.tenant_id,
                    subject_id: Some(subject.id),
                    subject_ref: subject_ref.clone(),
                    purpose: *purpose,
                    granted: false,
                    notice_version: notice_version.clone(),
                    method: ConsentMethod::OptOut,
                    evidence: serde_json::json!({}),
                    locale: None,
                    jurisdiction: subject.jurisdiction.clone(),
                    signal_source: Some(source),
                    recorded_at: now,
                    withdrawn_at: None,
                },
            )
            .await?;

        // The materialised row: the read path every other context uses.
        let directive = PurposeDirective {
            purpose: *purpose,
            state: DirectiveState::Denied,
            source: DirectiveSource::OptOut,
            jurisdiction: subject.jurisdiction.clone(),
            notice_version: Some(notice_version.clone()),
            effective_at: now,
            superseded_at: None,
        };
        directives.upsert(scope, &directive, subject_ref).await?;
    }
    let _ = (profile, signals);
    Ok(())
}

/// Resolve and persist a subject's jurisdiction.
pub struct ResolveJurisdiction {
    tx_manager: TxManager,
    subjects: Arc<dyn SubjectRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl ResolveJurisdiction {
    pub fn new(
        tx_manager: TxManager,
        subjects: Arc<dyn SubjectRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            subjects,
            jurisdictions,
            events,
            clock,
        }
    }

    #[tracing::instrument(name = "privacy.resolve_jurisdiction", skip_all)]
    pub async fn execute(
        &self,
        subject_id: SubjectId,
        evidence: Vec<(EvidenceKind, String)>,
    ) -> Result<String, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_resolve_jurisdiction", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let mut subject = self
            .subjects
            .find_by_id(&mut scope, subject_id)
            .await?
            .ok_or(PrivacyError::SubjectNotFound(
                subject_id.as_i64().to_string(),
            ))?;
        let (jurisdiction, chain) = resolve(&evidence);
        self.jurisdictions
            .find(&mut scope, jurisdiction.as_str())
            .await?
            .ok_or_else(|| PrivacyError::JurisdictionNotFound(jurisdiction.as_str().to_string()))?;
        subject.jurisdiction = jurisdiction.clone();
        subject.jurisdiction_evidence = chain;
        subject.updated_at = self.clock.now();
        self.subjects.save(&mut scope, &subject).await?;
        self.events
            .publish(
                &mut scope,
                &[PrivacyEvent::JurisdictionResolved {
                    subject: subject.subject_ref.clone(),
                    jurisdiction: jurisdiction.as_str().to_string(),
                }],
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(jurisdiction.as_str().to_string())
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GetDirectivesOutput {
    pub snapshot: DirectiveSnapshot,
}

/// The `DirectiveResolver` port implementation: reads the ledger and the
/// materialised rows, resolves via the pure engine and writes the result
/// back. Never returns "unknown".
pub struct PersistedDirectiveResolver {
    tx_manager: TxManager,
    subjects: Arc<dyn SubjectRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    consents: Arc<dyn ConsentRepository>,
    signals: Arc<dyn SignalRepository>,
    directives: Arc<dyn DirectiveRepository>,
    clock: Arc<dyn Clock>,
}

impl PersistedDirectiveResolver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        subjects: Arc<dyn SubjectRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        consents: Arc<dyn ConsentRepository>,
        signals: Arc<dyn SignalRepository>,
        directives: Arc<dyn DirectiveRepository>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            subjects,
            jurisdictions,
            consents,
            signals,
            directives,
            clock,
        }
    }

    async fn resolve_snapshot_impl(
        &self,
        subject: &SubjectRef,
    ) -> Result<DirectiveSnapshot, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_directive_resolve", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        // The subject's jurisdiction: persisted evidence or most protective.
        let stored = self
            .subjects
            .find_by_key(&mut scope, subject.canonical_key())
            .await?;
        let (jurisdiction, is_minor, subject_id) = match &stored {
            Some(stored) => (
                stored.jurisdiction.clone(),
                stored.is_minor,
                Some(stored.id),
            ),
            None => (crate::domain::most_protective(), false, None),
        };
        let profile = self
            .jurisdictions
            .find(&mut scope, jurisdiction.as_str())
            .await?
            .unwrap_or_else(crate::domain::most_protective_profile);

        // Explicit ledger inputs (latest per purpose).
        let explicit: Vec<DirectiveInput> = {
            let mut inputs = Vec::new();
            for purpose in ProcessingPurpose::ALL {
                if let Some(record) = self
                    .consents
                    .latest_for_purpose(&mut scope, subject_id, subject.canonical_key(), purpose)
                    .await?
                {
                    let state = if record.granted {
                        DirectiveState::Allowed
                    } else {
                        DirectiveState::Denied
                    };
                    let source = match record.method {
                        ConsentMethod::GuardianConsent => DirectiveSource::Guardian,
                        ConsentMethod::Consent => DirectiveSource::Consent,
                        ConsentMethod::OptOut | ConsentMethod::Withdrawal => {
                            DirectiveSource::OptOut
                        }
                    };
                    inputs.push(DirectiveInput {
                        purpose,
                        state,
                        source,
                        notice_version: Some(record.notice_version),
                    });
                }
            }
            inputs
        };

        // Universal opt-out signals: pre-auth device rows (device-scoped
        // reads) and bound rows (authenticated subjects — binding never
        // unbinds).
        let mut signals: Vec<DirectiveInput> = Vec::new();
        match subject.kind {
            crate::domain::SubjectKind::Device => {
                let device_ref = subject
                    .canonical_key()
                    .strip_prefix("device:")
                    .unwrap_or("");
                if let Some(signal) = self
                    .signals
                    .latest_for_device(&mut scope, device_ref)
                    .await?
                {
                    for purpose in &signal.purposes {
                        signals.push(DirectiveInput {
                            purpose: *purpose,
                            state: DirectiveState::Denied,
                            source: DirectiveSource::Signal,
                            notice_version: None,
                        });
                    }
                }
            }
            _ => {
                if let Some(subject_id) = subject_id {
                    for signal in self
                        .signals
                        .list_bound_for_subject(&mut scope, subject_id)
                        .await?
                    {
                        for purpose in &signal.purposes {
                            signals.push(DirectiveInput {
                                purpose: *purpose,
                                state: DirectiveState::Denied,
                                source: DirectiveSource::Signal,
                                notice_version: None,
                            });
                        }
                    }
                }
            }
        }

        let now = self.clock.now();
        let snapshot_directives =
            resolve_snapshot(&profile, &explicit, &signals, is_minor, None, now, None);

        // Materialise.
        for directive in &snapshot_directives {
            self.directives
                .upsert(&mut scope, directive, subject)
                .await?;
        }

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(DirectiveSnapshot {
            subject: subject.clone(),
            jurisdiction: profile.code,
            directives: snapshot_directives,
            minor_opt_in_age: profile.minor_opt_in_age,
            honours_universal_opt_out: profile.honours_universal_opt_out,
        })
    }
}

#[async_trait]
impl DirectiveResolver for PersistedDirectiveResolver {
    async fn resolve(
        &self,
        subject: &SubjectRef,
        purpose: ProcessingPurpose,
    ) -> Result<PurposeDirective, PrivacyError> {
        let snapshot = self.resolve_snapshot_impl(subject).await?;
        snapshot
            .directives
            .into_iter()
            .find(|directive| directive.purpose == purpose)
            .ok_or_else(|| {
                PrivacyError::InvalidPurpose(format!("no directive for {}", purpose.as_str()))
            })
    }

    async fn snapshot(&self, subject: &SubjectRef) -> Result<DirectiveSnapshot, PrivacyError> {
        self.resolve_snapshot_impl(subject).await
    }
}

pub struct GetDirectives {
    resolver: Arc<dyn DirectiveResolver>,
    signals: Arc<dyn SignalRepository>,
    subjects: Arc<dyn SubjectRepository>,
    tx_manager: TxManager,
}

impl GetDirectives {
    pub fn new(
        resolver: Arc<dyn DirectiveResolver>,
        signals: Arc<dyn SignalRepository>,
        subjects: Arc<dyn SubjectRepository>,
        tx_manager: TxManager,
    ) -> Self {
        Self {
            resolver,
            signals,
            subjects,
            tx_manager,
        }
    }

    /// Resolve the snapshot for `subject`, first binding any unbound
    /// preference signals recorded for its identifiers.
    #[tracing::instrument(name = "privacy.get_directives", skip_all)]
    pub async fn execute(&self, subject: &SubjectRef) -> Result<GetDirectivesOutput, PrivacyError> {
        self.bind(subject).await?;
        let snapshot = self.resolver.snapshot(subject).await?;
        Ok(GetDirectivesOutput { snapshot })
    }

    async fn bind(&self, subject: &SubjectRef) -> Result<(), PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_directive_bind", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let stored = self
            .subjects
            .find_by_key(&mut scope, subject.canonical_key())
            .await?;
        let Some(stored) = stored else {
            let _ = scope.commit().await;
            return Ok(());
        };
        let unbound = self
            .signals
            .unbound_for_subject(&mut scope, subject)
            .await?;
        for signal in unbound {
            self.signals
                .bind(
                    &mut scope,
                    signal.id,
                    stored.id,
                    stored.jurisdiction.as_str(),
                )
                .await?;
        }
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

/// Bind unbound signals for a subject (also invoked from the GPC
/// middleware's bind path).
pub struct BindUnboundSignals {
    inner: GetDirectives,
}

impl BindUnboundSignals {
    pub fn new(inner: GetDirectives) -> Self {
        Self { inner }
    }

    #[tracing::instrument(name = "privacy.bind_unbound_signals", skip_all)]
    pub async fn execute(&self, subject: &SubjectRef) -> Result<(), PrivacyError> {
        self.inner.bind(subject).await
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RecordConsentChangeCommand {
    pub purpose: ProcessingPurpose,
    pub granted: bool,
    pub notice_version: String,
    pub method: String,
    pub locale: Option<String>,
    pub signal_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RecordConsentChangeOutput {
    pub purpose: ProcessingPurpose,
    pub state: DirectiveState,
    pub source: DirectiveSource,
}

pub struct RecordConsentChange {
    tx_manager: TxManager,
    subjects: Arc<dyn SubjectRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    consents: Arc<dyn ConsentRepository>,
    directives: Arc<dyn DirectiveRepository>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl RecordConsentChange {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        subjects: Arc<dyn SubjectRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        consents: Arc<dyn ConsentRepository>,
        directives: Arc<dyn DirectiveRepository>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            subjects,
            jurisdictions,
            consents,
            directives,
            events,
            ids,
            clock,
        }
    }

    #[tracing::instrument(name = "privacy.record_consent_change", skip_all, fields(tenant_id = ?subject.tenant_id))]
    pub async fn execute(
        &self,
        subject: &SubjectRef,
        command: RecordConsentChangeCommand,
    ) -> Result<RecordConsentChangeOutput, PrivacyError> {
        if command.purpose == ProcessingPurpose::Essential {
            return Err(PrivacyError::EssentialNotConsentable);
        }
        let method = ConsentMethod::parse(&command.method)?;
        let signal_source = command
            .signal_source
            .as_deref()
            .map(SignalSource::parse)
            .transpose()?;

        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_consent_change", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let stored = self
            .subjects
            .find_by_key(&mut scope, subject.canonical_key())
            .await?
            .ok_or_else(|| PrivacyError::SubjectNotFound(subject.canonical_key().to_string()))?;
        let profile = self
            .jurisdictions
            .find(&mut scope, stored.jurisdiction.as_str())
            .await?
            .unwrap_or_else(crate::domain::most_protective_profile);

        let now = self.clock.now();
        let record = ConsentRecord {
            id: self.ids.next_consent_id()?,
            tenant_id: subject.tenant_id,
            subject_id: Some(stored.id),
            subject_ref: subject.clone(),
            purpose: command.purpose,
            granted: command.granted,
            notice_version: command.notice_version.clone(),
            method,
            evidence: serde_json::json!({}),
            locale: command.locale.clone(),
            jurisdiction: stored.jurisdiction.clone(),
            signal_source,
            recorded_at: now,
            withdrawn_at: if !command.granted { Some(now) } else { None },
        };
        self.consents.append(&mut scope, &record).await?;

        let explicit = DirectiveInput {
            purpose: command.purpose,
            state: if command.granted {
                DirectiveState::Allowed
            } else {
                DirectiveState::Denied
            },
            source: match method {
                ConsentMethod::GuardianConsent => DirectiveSource::Guardian,
                ConsentMethod::Consent => DirectiveSource::Consent,
                ConsentMethod::OptOut | ConsentMethod::Withdrawal => DirectiveSource::OptOut,
            },
            notice_version: Some(command.notice_version.clone()),
        };
        let directive = crate::domain::resolve_purpose(
            &profile,
            command.purpose,
            Some(&explicit),
            None,
            stored.is_minor,
            Some(&command.notice_version),
            now,
        );
        self.directives
            .upsert(&mut scope, &directive, subject)
            .await?;

        let event = if command.granted {
            PrivacyEvent::ConsentGranted {
                subject: subject.clone(),
                purpose: command.purpose,
                notice_version: command.notice_version.clone(),
                method: method.as_str().to_string(),
                jurisdiction: stored.jurisdiction.as_str().to_string(),
            }
        } else {
            PrivacyEvent::ConsentWithdrawn {
                subject: subject.clone(),
                purpose: command.purpose,
                notice_version: command.notice_version,
                jurisdiction: stored.jurisdiction.as_str().to_string(),
            }
        };
        self.events.publish(&mut scope, &[event]).await?;

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(RecordConsentChangeOutput {
            purpose: command.purpose,
            state: directive.state,
            source: directive.source,
        })
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RecordOptOutCommand {
    pub purposes: Vec<ProcessingPurpose>,
    /// `ui` | `api` | `gpc` — the GPC header path supplies `gpc`.
    pub source: String,
    pub device_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RecordOptOutOutput {
    pub directives: Vec<PurposeDirective>,
    pub jurisdiction: String,
}

/// The opt-out endpoint: no verification gate, immediate effect, immediate
/// materialisation. This is the CCPA-critical path — nothing may block it.
#[derive(Clone)]
pub struct RecordOptOut {
    tx_manager: TxManager,
    subjects: Arc<dyn SubjectRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    consents: Arc<dyn ConsentRepository>,
    signals: Arc<dyn SignalRepository>,
    directives: Arc<dyn DirectiveRepository>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    notice_version: String,
}

#[allow(clippy::too_many_arguments)]
impl RecordOptOut {
    pub fn new(
        tx_manager: TxManager,
        subjects: Arc<dyn SubjectRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        consents: Arc<dyn ConsentRepository>,
        signals: Arc<dyn SignalRepository>,
        directives: Arc<dyn DirectiveRepository>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        notice_version: String,
    ) -> Self {
        Self {
            tx_manager,
            subjects,
            jurisdictions,
            consents,
            signals,
            directives,
            events,
            ids,
            clock,
            notice_version,
        }
    }

    #[tracing::instrument(name = "privacy.record_opt_out", skip_all, fields(tenant_id = ?subject.tenant_id))]
    pub async fn execute(
        &self,
        subject: &SubjectRef,
        command: RecordOptOutCommand,
    ) -> Result<RecordOptOutOutput, PrivacyError> {
        let source = SignalSource::parse(&command.source)?;
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_opt_out", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let stored = self
            .subjects
            .find_by_key(&mut scope, subject.canonical_key())
            .await?;
        let (jurisdiction, profile) = match &stored {
            Some(stored) => {
                let profile = self
                    .jurisdictions
                    .find(&mut scope, stored.jurisdiction.as_str())
                    .await?
                    .unwrap_or_else(crate::domain::most_protective_profile);
                (stored.jurisdiction.clone(), profile)
            }
            // Pre-authentication device opt-outs: no subject row yet — the
            // device signal is authoritative and the jurisdiction resolves
            // on binding.
            None => {
                let jurisdiction = crate::domain::most_protective();
                (jurisdiction, crate::domain::most_protective_profile())
            }
        };

        // Record a device signal when the caller supplied a device ref (the
        // GPC header path); binding to a subject happens on the next
        // authenticated request.
        if let Some(device_ref) = &command.device_ref {
            let signal = crate::domain::PreferenceSignal {
                id: self.ids.next_preference_signal_id()?,
                tenant_id: subject.tenant_id,
                signal: "gpc".to_string(),
                device_ref: device_ref.clone(),
                subject_id: stored.as_ref().map(|s| s.id),
                jurisdiction: Some(jurisdiction.clone()),
                purposes: command.purposes.clone(),
                observed_at: self.clock.now(),
                bound_at: stored.as_ref().map(|_| self.clock.now()),
                evidence: serde_json::json!({
                    "identifier": subject.identifiers.first().map(|i| i.value.clone()),
                }),
            };
            self.signals.append(&mut scope, &signal).await?;
        }

        let mut resolved = Vec::new();
        if let Some(stored) = &stored {
            let purposes: Vec<ProcessingPurpose> = if command.purposes.is_empty() {
                ProcessingPurpose::UNIVERSAL_OPT_OUT.to_vec()
            } else {
                command.purposes.clone()
            };
            flip_directives(
                &mut scope,
                self.consents.as_ref(),
                self.directives.as_ref(),
                self.signals.as_ref(),
                self.ids.as_ref(),
                stored,
                &profile,
                subject,
                &purposes,
                Some(&self.notice_version),
                self.clock.now(),
                source,
            )
            .await?;
            for purpose in &purposes {
                resolved.push(PurposeDirective {
                    purpose: *purpose,
                    state: DirectiveState::Denied,
                    source: DirectiveSource::OptOut,
                    jurisdiction: jurisdiction.clone(),
                    notice_version: Some(self.notice_version.clone()),
                    effective_at: self.clock.now(),
                    superseded_at: None,
                });
            }
        } else {
            // Device-level resolution: the directive engine denies the
            // purposes from the signal row on the next read.
            for purpose in command.purposes.clone() {
                let (state, source) = jurisdiction_default(&profile, purpose);
                resolved.push(PurposeDirective {
                    purpose,
                    state,
                    source,
                    jurisdiction: jurisdiction.clone(),
                    notice_version: Some(self.notice_version.clone()),
                    effective_at: self.clock.now(),
                    superseded_at: None,
                });
            }
        }

        self.events
            .publish(
                &mut scope,
                &[PrivacyEvent::ProcessingOptedOut {
                    subject: subject.clone(),
                    purposes: command.purposes.clone(),
                    source: source.as_str().to_string(),
                    jurisdiction: jurisdiction.as_str().to_string(),
                }],
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(RecordOptOutOutput {
            directives: resolved,
            jurisdiction: jurisdiction.as_str().to_string(),
        })
    }
}

/// "Limit the use of my sensitive personal information" (CPRA).
pub struct RecordLimitSensitive {
    inner: RecordOptOut,
}

impl RecordLimitSensitive {
    pub fn new(inner: RecordOptOut) -> Self {
        Self { inner }
    }

    #[tracing::instrument(name = "privacy.record_limit_sensitive", skip_all)]
    pub async fn execute(
        &self,
        subject: &SubjectRef,
        source: String,
    ) -> Result<RecordOptOutOutput, PrivacyError> {
        self.inner
            .execute(
                subject,
                RecordOptOutCommand {
                    purposes: vec![ProcessingPurpose::SensitivePiUse],
                    source,
                    device_ref: None,
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jurisdiction_default_is_pure() {
        let eu =
            crate::domain::seeded_profile(&crate::domain::JurisdictionRef::parse("eu").unwrap())
                .unwrap();
        let (state, source) = jurisdiction_default(&eu, ProcessingPurpose::Analytics);
        assert_eq!(state, DirectiveState::Denied);
        assert_eq!(source, DirectiveSource::Default);
    }

    #[test]
    fn most_protective_is_eu() {
        assert_eq!(crate::domain::most_protective().as_str(), "eu");
    }
}
