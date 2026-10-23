//! The access context's `PersonalDataProvider`.
//!
//! Access holds two personal references: `access.entitlements.granted_by`
//! (the operator who granted an override) and `granted_reason` free text.
//! Membership projection checkpoints also reference users and tenant roles.
//! Roles carry no creator; quota usage is aggregate tenant data. Erasure
//! anonymises the granted-by references; Keto relation tuples for the user
//! are removed through the permission store (the access context's own
//! contract).

use async_trait::async_trait;
use sanvi_kernel::UserId;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

use crate::domain::{EntitlementRepository, PermissionStore};

pub struct AccessPrivacyProvider {
    tx_manager: TxManager,
    entitlements: std::sync::Arc<dyn EntitlementRepository>,
    store: std::sync::Arc<dyn PermissionStore>,
}

impl AccessPrivacyProvider {
    pub fn new(
        tx_manager: TxManager,
        entitlements: std::sync::Arc<dyn EntitlementRepository>,
        store: std::sync::Arc<dyn PermissionStore>,
    ) -> Self {
        Self {
            tx_manager,
            entitlements,
            store,
        }
    }

    fn user_id(subject: &SubjectRef) -> Option<UserId> {
        subject
            .identifiers
            .iter()
            .find(|id| id.id_type == "user_id")
            .and_then(|id| id.value.parse::<UserId>().ok())
    }
}

#[async_trait]
impl PersonalDataProvider for AccessPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "access"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| matches!(def.class, DataClass::Membership))
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let Some(user_id) = Self::user_id(subject) else {
            return Ok(DataBundle {
                context: "access".to_string(),
                entries: Vec::new(),
            });
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("access_privacy_provider", "access")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let grants = self
            .entitlements
            .list_by_granted_by(&mut scope, user_id)
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let checkpoints = sqlx::query!("SELECT tenant_id, role_objects FROM access.membership_projection_checkpoints WHERE user_id = $1 ORDER BY tenant_id", user_id.as_i64())
            .fetch_all(&mut **scope.tx_mut().as_inner()).await.map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let mut entries: Vec<DataEntry> = grants
            .into_iter()
            .map(|grant| DataEntry {
                data_class: DataClass::Membership,
                category: "Account".to_string(),
                label: "entitlement_override".to_string(),
                value: serde_json::json!({
                    "tenant_id": grant.tenant_id(),
                    "feature": grant.feature().as_str(),
                    "enabled": grant.enabled(),
                }),
            })
            .collect();
        entries.extend(checkpoints.into_iter().map(|checkpoint| DataEntry {
            data_class: DataClass::Membership,
            category: "Account".to_string(),
            label: "authorization_projection".to_string(),
            value: serde_json::json!({"tenant_id": checkpoint.tenant_id.to_string(), "role_objects": checkpoint.role_objects}),
        }));
        Ok(DataBundle {
            context: "access".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let mut anonymised = 0;
        let mut erased = 0;
        if let Some(user_id) = Self::user_id(subject) {
            let mut scope = self
                .tx_manager
                .begin_unscoped("access_privacy_erase", "access")
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            anonymised = self
                .entitlements
                .anonymise_granted_by(&mut scope, user_id)
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            let checkpoints = sqlx::query!("SELECT tenant_id, role_objects FROM access.membership_projection_checkpoints WHERE user_id = $1 ORDER BY tenant_id", user_id.as_i64())
                .fetch_all(&mut **scope.tx_mut().as_inner()).await.map_err(|err| PrivacyError::Storage(err.to_string()))?;
            for checkpoint in &checkpoints {
                let mut tuples: Vec<crate::domain::Tuple> = checkpoint
                    .role_objects
                    .iter()
                    .map(|object| crate::domain::Tuple::role_membership(object, user_id))
                    .collect();
                let tenant = sanvi_kernel::TenantId::try_from(checkpoint.tenant_id)
                    .map_err(|err| PrivacyError::Storage(err.to_string()))?;
                tuples.push(crate::domain::Tuple::tenant_membership(tenant, user_id));
                self.store
                    .delete(&tuples)
                    .await
                    .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            }
            erased += sqlx::query!(
                "DELETE FROM access.membership_projection_checkpoints WHERE user_id = $1",
                user_id.as_i64()
            )
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?
            .rows_affected() as usize;
            // Keto relation tuples referencing the user (tenant containment;
            // role memberships die with the membership rows the identity
            // provider removes, and the access sync consumer drops the
            // tuples from the identity events).
            if let Some(tenant_id) = subject.tenant_id {
                let _ = self
                    .store
                    .delete(&[crate::domain::Tuple::tenant_membership(tenant_id, user_id)])
                    .await;
                erased += 1;
            }
            scope
                .commit()
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        }
        Ok(EraseReport {
            context: "access".to_string(),
            erased,
            anonymised,
            retained: vec![RetainedItem {
                data_class: DataClass::Membership,
                reason: RetentionReason::Operational,
                detail: "entitlement rows themselves are tenant access records; only the operator references are anonymised".to_string(),
            }],
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        // No subject-owned field to correct: reasons are evidence.
        Ok(())
    }
}
