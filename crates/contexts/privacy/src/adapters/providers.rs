//! The privacy context's own `PersonalDataProvider`: consent ledger rows,
//! preference signals, directive materialisations, jurisdiction evidence and
//! request records. Erasure anonymises the compliance records (proof of
//! consent must outlive the consent; the subject's identifiers are replaced
//! with a pseudonym) and deletes the device signals.

use async_trait::async_trait;

use sanvi_persistence::TxManager;

use crate::domain::{
    ConsentRepository, DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry,
    DirectiveRepository, EraseMode, EraseReport, PersonalDataProvider, PrivacyError, RectifyPatch,
    RetentionReason, SignalRepository, SubjectRef, SubjectRepository,
};

pub struct PrivacyContextProvider {
    tx_manager: TxManager,
    subjects: std::sync::Arc<dyn SubjectRepository>,
    consents: std::sync::Arc<dyn ConsentRepository>,
    signals: std::sync::Arc<dyn SignalRepository>,
    directives: std::sync::Arc<dyn DirectiveRepository>,
}

impl PrivacyContextProvider {
    pub fn new(
        tx_manager: TxManager,
        subjects: std::sync::Arc<dyn SubjectRepository>,
        consents: std::sync::Arc<dyn ConsentRepository>,
        signals: std::sync::Arc<dyn SignalRepository>,
        directives: std::sync::Arc<dyn DirectiveRepository>,
    ) -> Self {
        Self {
            tx_manager,
            subjects,
            consents,
            signals,
            directives,
        }
    }

    async fn scope(&self) -> Result<sanvi_persistence::PlatformScope<'_>, PrivacyError> {
        self.tx_manager
            .begin_unscoped("privacy_provider", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))
    }
}

fn entries_for(bundles: &mut Vec<DataEntry>, subject: &SubjectRef) {
    bundles.push(DataEntry {
        data_class: DataClass::Jurisdiction,
        category: "Compliance".to_string(),
        label: "subject_key".to_string(),
        value: serde_json::json!(subject.canonical_key()),
    });
}

#[async_trait]
impl PersonalDataProvider for PrivacyContextProvider {
    fn context_name(&self) -> &'static str {
        "privacy"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| {
                matches!(
                    def.class,
                    DataClass::ConsentRecord
                        | DataClass::Preference
                        | DataClass::Jurisdiction
                        | DataClass::RequestRecord
                )
            })
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let mut scope = self.scope().await?;
        let mut entries = Vec::new();
        entries_for(&mut entries, subject);

        if let Some(stored) = self
            .subjects
            .find_by_key(&mut scope, subject.canonical_key())
            .await?
        {
            entries.push(DataEntry {
                data_class: DataClass::Jurisdiction,
                category: "Compliance".to_string(),
                label: "jurisdiction".to_string(),
                value: serde_json::json!(stored.jurisdiction.as_str()),
            });
            entries.push(DataEntry {
                data_class: DataClass::Jurisdiction,
                category: "Compliance".to_string(),
                label: "jurisdiction_evidence".to_string(),
                value: serde_json::to_value(&stored.jurisdiction_evidence).unwrap_or_default(),
            });
            entries.push(DataEntry {
                data_class: DataClass::Jurisdiction,
                category: "Compliance".to_string(),
                label: "known_minor".to_string(),
                value: serde_json::json!(stored.is_minor),
            });
        }

        for record in self
            .consents
            .list_for_subject(&mut scope, None, subject.canonical_key())
            .await?
        {
            entries.push(DataEntry {
                data_class: DataClass::ConsentRecord,
                category: "Compliance".to_string(),
                label: format!("consent:{}:{}", record.purpose.as_str(), record.recorded_at),
                value: serde_json::json!({
                    "purpose": record.purpose.as_str(),
                    "granted": record.granted,
                    "method": record.method.as_str(),
                    "notice_version": record.notice_version,
                    "recorded_at": record.recorded_at,
                    "jurisdiction": record.jurisdiction.as_str(),
                }),
            });
        }

        let directives = self
            .directives
            .list(&mut scope, subject.canonical_key())
            .await?;
        for directive in directives {
            entries.push(DataEntry {
                data_class: DataClass::Preference,
                category: "Compliance".to_string(),
                label: format!("directive:{}", directive.purpose.as_str()),
                value: serde_json::json!({
                    "state": directive.state.as_str(),
                    "source": directive.source.as_str(),
                    "jurisdiction": directive.jurisdiction.as_str(),
                }),
            });
        }

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(DataBundle {
            context: "privacy".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let mut scope = self.scope().await?;
        let mut erased = 0;

        // Consent ledger: proof of consent must outlive the consent. The
        // subject's identifiers are replaced with a pseudonym; the
        // purpose/method/timestamp/jurisdiction survive for audit.
        let anonymised = anonymised_subject_ref(subject);
        let consents_anonymised = self
            .consents
            .anonymise_subject(&mut scope, subject.canonical_key(), &anonymised)
            .await?;

        // Device signals are deleted outright (no consent, no proof duty).
        erased += self.signals.delete_unbound(&mut scope, subject).await?;

        // Directives are superseded (a tombstone, not a deletion, so the
        // "denied" state survives erasure and re-registration).
        self.directives
            .supersede_all(&mut scope, subject.canonical_key())
            .await?;

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(EraseReport {
            context: "privacy".to_string(),
            erased,
            anonymised: consents_anonymised,
            retained: vec![
                crate::domain::RetainedItem {
                    data_class: DataClass::ConsentRecord,
                    reason: RetentionReason::StatutoryRetention,
                    detail: format!(
                        "{consents_anonymised} consent ledger rows anonymised: proof of consent/opt-out retained as pseudonymised records"
                    ),
                },
                crate::domain::RetainedItem {
                    data_class: DataClass::RequestRecord,
                    reason: RetentionReason::StatutoryRetention,
                    detail:
                        "request records retained (anonymised) for regulatory metrics and audit"
                            .to_string(),
                },
            ],
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        // Compliance records are never rectified: they are evidence.
        Ok(())
    }
}

/// Anonymise helper shared with the erasure executor: replace the subject
/// reference with a pseudonym derived from the canonical key (deterministic
/// so re-anonymisation is idempotent and the audit chain stays linkable).
pub fn pseudonym(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    format!("anon-{}", hex_prefix(&digest))
}

fn hex_prefix(digest: &[u8]) -> String {
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Generate the anonymised subject_ref used when rewriting rows.
pub fn anonymised_subject_ref(subject: &SubjectRef) -> SubjectRef {
    SubjectRef {
        kind: subject.kind,
        tenant_id: None,
        key: pseudonym(subject.canonical_key()),
        identifiers: vec![],
    }
}

/// Whether a subject key is a pseudonym (post-erasure rows).
pub fn is_pseudonym(key: &str) -> bool {
    key.starts_with("anon-")
}
