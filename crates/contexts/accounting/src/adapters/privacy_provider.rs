//! Privacy participation for organization-level integration metadata.
//!
//! `oauth_states` and `audit_commands` hold acting user IDs and are covered by
//! the review requirement. No customer identifiers or provider credentials are
//! stored in these tables. Organization references may still identify a sole
//! proprietor. When tenant-user requests touch existing metadata, require explicit
//! subject/entity review instead of exporting another member's company settings,
//! claiming blanket statutory retention, or silently deleting reconciliation history.

use async_trait::async_trait;
use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, SubjectKind, SubjectRef,
};

pub struct AccountingPrivacyProvider {
    transactions: TxManager,
}

impl AccountingPrivacyProvider {
    pub fn new(transactions: TxManager) -> Self {
        Self { transactions }
    }

    async fn require_entity_review(&self, subject: &SubjectRef) -> Result<(), PrivacyError> {
        if subject.kind != SubjectKind::TenantUser {
            return Ok(());
        }
        let Some(tenant) = subject.tenant_id else {
            return Ok(());
        };
        let mut scope = self
            .transactions
            .begin_for(tenant.as_i64())
            .await
            .map_err(|error| PrivacyError::Storage(error.to_string()))?;
        let exists = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM accounting.books WHERE tenant_id = $1) AS \"exists!\"",
            tenant.as_i64()
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|error| PrivacyError::Storage(error.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|error| PrivacyError::Storage(error.to_string()))?;
        if exists {
            return Err(PrivacyError::ProcessingPending("accounting organization metadata requires subject/legal-entity review and explicit fulfillment before this provider can complete".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl PersonalDataProvider for AccountingPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "accounting"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|definition| definition.class == DataClass::Billing)
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        self.require_entity_review(subject).await?;
        Ok(DataBundle {
            context: "accounting".into(),
            entries: Vec::new(),
        })
    }

    async fn erase(&self, subject: &SubjectRef, _: EraseMode) -> Result<EraseReport, PrivacyError> {
        self.require_entity_review(subject).await?;
        Ok(EraseReport {
            context: "accounting".into(),
            erased: 0,
            anonymised: 0,
            retained: Vec::new(),
        })
    }

    async fn rectify(
        &self,
        subject: &SubjectRef,
        patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        if patch
            .corrections
            .iter()
            .any(|correction| correction.context == "accounting")
        {
            self.require_entity_review(subject).await?;
        }
        Ok(())
    }
}

mod service {
    use crate::adapters::remote::{
        Delegation, RemoteClient, RequestBinding, ServiceConfig, TENANT_AUDIENCE,
    };
    use async_trait::async_trait;
    use sanvi_privacy::domain::{
        DATA_CLASSES, DataBundle, DataClass, DataClassDef, EraseMode, EraseReport,
        PersonalDataProvider, PrivacyError, RectifyPatch, SubjectKind, SubjectRef,
    };

    pub struct RemoteAccountingPrivacyProvider {
        client: RemoteClient,
    }
    impl RemoteAccountingPrivacyProvider {
        pub fn new(config: &ServiceConfig) -> anyhow::Result<Self> {
            Ok(Self {
                client: RemoteClient::new(config)?,
            })
        }
        async fn review(&self, subject: &SubjectRef) -> Result<(), PrivacyError> {
            if subject.kind != SubjectKind::TenantUser || subject.tenant_id.is_none() {
                return Ok(());
            }
            let body =
                serde_json::to_vec(subject).map_err(|e| PrivacyError::Storage(e.to_string()))?;
            let claims = Delegation::new(
                TENANT_AUDIENCE,
                "privacy",
                &RequestBinding::json("POST", "/internal/privacy/review", &body),
            );
            let result = self
                .client
                .send(&claims, body, "application/json", "")
                .await
                .map_err(|_| {
                    PrivacyError::ProcessingPending(
                        "Accounting service unavailable; privacy review is pending".into(),
                    )
                })?;
            match result.status {
            204 => Ok(()),
            409 => Err(PrivacyError::ProcessingPending("accounting organization metadata requires subject/legal-entity review and explicit fulfillment before this provider can complete".into())),
            _ => Err(PrivacyError::ProcessingPending("Accounting service privacy review failed".into())),
        }
        }
    }
    #[async_trait]
    impl PersonalDataProvider for RemoteAccountingPrivacyProvider {
        fn context_name(&self) -> &'static str {
            "accounting"
        }
        fn data_classes(&self) -> Vec<DataClassDef> {
            DATA_CLASSES
                .iter()
                .filter(|d| d.class == DataClass::Billing)
                .copied()
                .collect()
        }
        async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
            self.review(subject).await?;
            Ok(DataBundle {
                context: "accounting".into(),
                entries: Vec::new(),
            })
        }
        async fn erase(
            &self,
            subject: &SubjectRef,
            _: EraseMode,
        ) -> Result<EraseReport, PrivacyError> {
            self.review(subject).await?;
            Ok(EraseReport {
                context: "accounting".into(),
                erased: 0,
                anonymised: 0,
                retained: Vec::new(),
            })
        }
        async fn rectify(
            &self,
            subject: &SubjectRef,
            patch: &RectifyPatch,
        ) -> Result<(), PrivacyError> {
            if patch.corrections.iter().any(|c| c.context == "accounting") {
                self.review(subject).await?;
            }
            Ok(())
        }
    }
}
pub use service::RemoteAccountingPrivacyProvider;
