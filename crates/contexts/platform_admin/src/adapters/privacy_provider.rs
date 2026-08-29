//! The platform_admin context's `PersonalDataProvider`.
//!
//! Personal references: impersonation grants (actor + target user ids) and
//! pending approvals (requester + decider ids, plus free-text payloads).
//! Both tables have no DELETE grants — erasure is a soft revoke / reference
//! anonymisation, and the report says so.

use async_trait::async_trait;
use sanvi_kernel::UserId;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

use crate::domain::{ImpersonationGrantRepository, PendingApprovalRepository, PrivacyLock};

pub struct PlatformAdminPrivacyProvider {
    tx_manager: TxManager,
    grants: std::sync::Arc<dyn ImpersonationGrantRepository>,
    approvals: std::sync::Arc<dyn PendingApprovalRepository>,
    _privacy_lock: std::sync::Arc<dyn PrivacyLock>,
}

impl PlatformAdminPrivacyProvider {
    pub fn new(
        tx_manager: TxManager,
        grants: std::sync::Arc<dyn ImpersonationGrantRepository>,
        approvals: std::sync::Arc<dyn PendingApprovalRepository>,
        privacy_lock: std::sync::Arc<dyn PrivacyLock>,
    ) -> Self {
        Self {
            tx_manager,
            grants,
            approvals,
            _privacy_lock: privacy_lock,
        }
    }

    fn user_id(subject: &SubjectRef) -> Option<UserId> {
        subject
            .identifiers
            .iter()
            .find(|id| id.id_type == "user_id")
            .and_then(|id| id.value.parse().ok())
    }
}

#[async_trait]
impl PersonalDataProvider for PlatformAdminPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "platform_admin"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| matches!(def.class, DataClass::AuditLog))
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let Some(user_id) = Self::user_id(subject) else {
            return Ok(DataBundle {
                context: "platform_admin".to_string(),
                entries: Vec::new(),
            });
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("platform_admin_privacy_provider", "platform_admin")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let mut entries = Vec::new();
        for grant in self
            .grants
            .list_by_actor(&mut scope, user_id)
            .await
            .map_err(storage)?
        {
            entries.push(DataEntry {
                data_class: DataClass::AuditLog,
                category: "Security".to_string(),
                label: "impersonation_grant".to_string(),
                value: serde_json::json!({
                    "tenant_id": grant.tenant_id(),
                    "target_user_id": grant.target_user_id(),
                    "created_at": grant.created_at(),
                }),
            });
        }
        for grant in self
            .grants
            .list_by_target(&mut scope, user_id)
            .await
            .map_err(storage)?
        {
            entries.push(DataEntry {
                data_class: DataClass::AuditLog,
                category: "Security".to_string(),
                label: "impersonation_grant_received".to_string(),
                value: serde_json::json!({
                    "tenant_id": grant.tenant_id(),
                    "actor_id": grant.actor_id(),
                    "created_at": grant.created_at(),
                }),
            });
        }
        for approval in self
            .approvals
            .list_by_requester(&mut scope, user_id)
            .await
            .map_err(storage)?
        {
            entries.push(DataEntry {
                data_class: DataClass::AuditLog,
                category: "Security".to_string(),
                label: "pending_approval".to_string(),
                value: serde_json::json!({
                    "action_type": approval.action_type(),
                    "status": approval.status().as_str(),
                }),
            });
        }
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(DataBundle {
            context: "platform_admin".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let mut erased = 0;
        let mut anonymised = 0;
        if let Some(user_id) = Self::user_id(subject) {
            let mut scope = self
                .tx_manager
                .begin_unscoped("platform_admin_privacy_erase", "platform_admin")
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            erased += self
                .grants
                .revoke_all_for_actor(&mut scope, user_id)
                .await
                .map_err(storage)?;
            anonymised += self
                .grants
                .anonymise_target(&mut scope, user_id)
                .await
                .map_err(storage)?;
            anonymised += self
                .approvals
                .anonymise_for_user(&mut scope, user_id)
                .await
                .map_err(storage)?;
            scope
                .commit()
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        }
        Ok(EraseReport {
            context: "platform_admin".to_string(),
            erased,
            anonymised,
            retained: vec![RetainedItem {
                data_class: DataClass::AuditLog,
                reason: RetentionReason::StatutoryRetention,
                detail: "approval records retained as compliance evidence with requester/decider references pseudonymised".to_string(),
            }],
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        Ok(())
    }
}

fn storage(err: crate::domain::PlatformAdminError) -> PrivacyError {
    PrivacyError::Storage(err.to_string())
}
