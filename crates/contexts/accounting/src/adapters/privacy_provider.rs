//! Privacy participation for organization-level integration metadata.
//! The Accounting service owns `accounting.*`, so every provider call is
//! forwarded there and the embedded provider's real result comes back:
//! `POST /internal/privacy/review` under a single-use `privacy` delegation.

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
        let body = serde_json::to_vec(subject).map_err(|e| PrivacyError::Storage(e.to_string()))?;
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
            409 => Err(PrivacyError::ProcessingPending(
                "accounting organization metadata requires subject/legal-entity review and explicit fulfillment before this provider can complete".into(),
            )),
            _ => Err(PrivacyError::ProcessingPending(
                "Accounting service privacy review failed".into(),
            )),
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

    async fn erase(&self, subject: &SubjectRef, _: EraseMode) -> Result<EraseReport, PrivacyError> {
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
