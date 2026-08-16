//! The audit context's `PersonalDataProvider`.
//!
//! `audit.entries` is append-only and hash-chained: rows can neither be
//! updated nor deleted without breaking the chain, and the application role
//! holds no UPDATE/DELETE grants. The provider therefore *collects* the
//! subject's audit trail and *retains* it on erasure with an explicit,
//! honest explanation — anonymisation of audit references happens by
//! pseudonymising the identifiers *before they enter* the chain, which the
//! writing contexts do as part of their own erasure.

use async_trait::async_trait;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

use crate::domain::{ActorType, AuditEntryData, AuditFilters, AuditReader, AuditWriter};

pub struct AuditPrivacyProvider {
    tx_manager: TxManager,
    reader: std::sync::Arc<dyn AuditReader>,
    /// Kept to prove the provider could have written, but the writer is
    /// never used: the audit chain is append-only by design.
    _writer: std::sync::Arc<dyn AuditWriter>,
}

impl AuditPrivacyProvider {
    pub fn new(
        tx_manager: TxManager,
        reader: std::sync::Arc<dyn AuditReader>,
        writer: std::sync::Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            reader,
            _writer: writer,
        }
    }
}

#[async_trait]
impl PersonalDataProvider for AuditPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "audit"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| matches!(def.class, DataClass::AuditLog))
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let actor_id = subject.identifiers.iter().find_map(|id| {
            (id.id_type == "user_id")
                .then(|| uuid::Uuid::parse_str(&id.value).ok())
                .flatten()
        });
        let Some(actor_id) = actor_id else {
            return Ok(DataBundle {
                context: "audit".to_string(),
                entries: Vec::new(),
            });
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("audit_privacy_provider", "audit")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let (entries, _) = self
            .reader
            .list(
                &mut scope,
                &AuditFilters {
                    tenant_id: subject.tenant_id,
                    actor_id: Some(actor_id),
                    action: None,
                    since: None,
                    until: None,
                },
                None,
                500,
                false,
            )
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        let mut data_entries = Vec::new();
        for entry in entries {
            data_entries.push(DataEntry {
                data_class: DataClass::AuditLog,
                category: "Security".to_string(),
                label: entry.action,
                value: serde_json::json!({
                    "occurred_at": entry.occurred_at,
                    "resource": entry.resource_type,
                    "tenant_id": entry.tenant_id,
                }),
            });
        }
        Ok(DataBundle {
            context: "audit".to_string(),
            entries: data_entries,
        })
    }

    async fn erase(
        &self,
        _subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        Ok(EraseReport {
            context: "audit".to_string(),
            erased: 0,
            anonymised: 0,
            retained: vec![RetainedItem {
                data_class: DataClass::AuditLog,
                reason: RetentionReason::StatutoryRetention,
                detail: "audit trail is append-only and hash-chained; identifiers are pseudonymised by the writing contexts during their own erasure and the chain is retained for its retention period".to_string(),
            }],
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        // The audit chain is immutable evidence; it is never rectified.
        Ok(())
    }
}

/// Adapts the audit context's writer onto the privacy context's audit port
/// (the privacy context defines the port; the audit context implements it —
/// no dependency cycle).
pub struct PrivacyAuditWriterAdapter {
    writer: std::sync::Arc<dyn AuditWriter>,
}

impl PrivacyAuditWriterAdapter {
    pub fn new(writer: std::sync::Arc<dyn AuditWriter>) -> Self {
        Self { writer }
    }
}

#[async_trait]
impl sanvi_privacy::domain::AuditWriter for PrivacyAuditWriterAdapter {
    async fn record(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        actor: uuid::Uuid,
        action: String,
        resource: String,
        detail: serde_json::Value,
    ) -> Result<(), sanvi_privacy::domain::PrivacyError> {
        let actor_type = if actor == uuid::Uuid::nil() {
            ActorType::System
        } else {
            ActorType::User
        };
        self.writer
            .append(
                scope,
                &AuditEntryData {
                    occurred_at: chrono::Utc::now(),
                    actor_type,
                    actor_id: Some(actor),
                    on_behalf_of: None,
                    tenant_id: None,
                    action,
                    resource_type: Some("privacy".to_string()),
                    resource_id: Some(resource),
                    before: None,
                    after: Some(detail),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| sanvi_privacy::domain::PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}
