//! The tenancy context's `PersonalDataProvider`.
//!
//! Tenancy holds the tenant account record (platform-account data, not
//! subject data): a `TenantUser` subject maps to the tenant row through
//! `subject.tenant_id`; an `EndUser` subject's tenant record is the
//! controller's account, which is *not* personal data of the end user.
//!
//! Erasure of a person never deletes the tenant account — that is the
//! tenancy lifecycle (suspend/archive/purge). The provider therefore
//! collects the account-level record for transparency and erases nothing:
//! its report is empty by design, and the honest explanation lives in the
//! retained items.

use async_trait::async_trait;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectKind,
    SubjectRef,
};

use crate::domain::{TenantId, TenantRepository};

pub struct TenancyPrivacyProvider {
    tx_manager: TxManager,
    tenants: std::sync::Arc<dyn TenantRepository>,
}

impl TenancyPrivacyProvider {
    pub fn new(tx_manager: TxManager, tenants: std::sync::Arc<dyn TenantRepository>) -> Self {
        Self {
            tx_manager,
            tenants,
        }
    }
}

#[async_trait]
impl PersonalDataProvider for TenancyPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "tenancy"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| matches!(def.class, DataClass::Contact))
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let mut entries = Vec::new();
        if let Some(tenant_id) = subject.tenant_id {
            let mut scope = self
                .tx_manager
                .begin_unscoped("tenancy_privacy_provider", "tenancy")
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            if let Some(tenant) = self
                .tenants
                .find_by_id(&mut scope, TenantId::from_uuid(tenant_id))
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?
            {
                entries.push(DataEntry {
                    data_class: DataClass::Contact,
                    category: "Account".to_string(),
                    label: "tenant_display_name".to_string(),
                    value: serde_json::json!(tenant.display_name().as_str()),
                });
                entries.push(DataEntry {
                    data_class: DataClass::Contact,
                    category: "Account".to_string(),
                    label: "tenant_slug".to_string(),
                    value: serde_json::json!(tenant.slug().as_str()),
                });
                entries.push(DataEntry {
                    data_class: DataClass::Contact,
                    category: "Account".to_string(),
                    label: "tenant_region".to_string(),
                    value: serde_json::json!(format!("{}", tenant.region())),
                });
            }
            scope
                .commit()
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        }
        Ok(DataBundle {
            context: "tenancy".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        // The tenant account belongs to the organisation, not the person.
        // Membership removal (identity) and entitlement anonymisation
        // (access) erase the person; the tenant row itself is account data
        // and its deletion is the tenancy lifecycle's job.
        let mut retained = Vec::new();
        if subject.kind == SubjectKind::TenantUser && subject.tenant_id.is_some() {
            retained.push(RetainedItem {
                data_class: DataClass::Contact,
                reason: RetentionReason::Operational,
                detail: "tenant account record retained: organisation account data, deleted by the tenant lifecycle (archive/purge), not by a subject erasure".to_string(),
            });
        }
        Ok(EraseReport {
            context: "tenancy".to_string(),
            erased: 0,
            anonymised: 0,
            retained,
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        // Tenant account corrections are tenant-settings operations, not
        // subject rectification.
        Ok(())
    }
}
