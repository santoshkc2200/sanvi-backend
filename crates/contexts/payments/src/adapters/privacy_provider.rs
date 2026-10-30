//! The Payments service owns `payments.*`, so every provider call is
//! forwarded there and the embedded provider's real result comes back:
//! `POST /internal/privacy/{collect,erase,rectify}` under a single-use
//! `privacy` delegation. An unreachable or unconfigured service, a 409, or
//! any other failure leaves the DSR pending; nothing is reported done
//! unless the service did it.

use crate::adapters::remote::{
    Delegation, RemoteClient, RequestBinding, ServiceConfig, TENANT_AUDIENCE,
};
use async_trait::async_trait;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, SubjectRef,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct RemotePaymentsPrivacyProvider {
    client: RemoteClient,
}

impl RemotePaymentsPrivacyProvider {
    pub fn new(config: &ServiceConfig) -> anyhow::Result<Self> {
        Ok(Self {
            client: RemoteClient::new(config)?,
        })
    }

    /// Send one provider call; `Some(body)` on 200, `None` on 204.
    async fn call<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &'static str,
        body: &B,
    ) -> Result<Option<T>, PrivacyError> {
        let body = serde_json::to_vec(body).map_err(|e| PrivacyError::Storage(e.to_string()))?;
        let claims = Delegation::new(
            TENANT_AUDIENCE,
            "privacy",
            &RequestBinding::json("POST", path, &body),
        );
        let response = self
            .client
            .send(&claims, body, "application/json", "")
            .await
            .map_err(|_| {
                PrivacyError::ProcessingPending(
                    "Payments service unavailable; privacy request is pending".into(),
                )
            })?;
        match response.status {
            200 => serde_json::from_slice(&response.body)
                .map(Some)
                .map_err(|_| {
                    PrivacyError::ProcessingPending(
                        "Payments service returned an unreadable privacy result".into(),
                    )
                }),
            204 => Ok(None),
            409 => Err(PrivacyError::ProcessingPending(
                "Payments service reports the request is pending".into(),
            )),
            status => Err(PrivacyError::ProcessingPending(format!(
                "Payments service privacy request failed with status {status}"
            ))),
        }
    }

    async fn call_for<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &'static str,
        body: &B,
    ) -> Result<T, PrivacyError> {
        self.call(path, body).await?.ok_or_else(|| {
            PrivacyError::ProcessingPending(format!("Payments service sent no result for {path}"))
        })
    }
}

#[derive(Serialize)]
struct EraseRequest<'a> {
    subject: &'a SubjectRef,
    mode: EraseMode,
}

#[derive(Serialize)]
struct RectifyRequest<'a> {
    subject: &'a SubjectRef,
    patch: &'a RectifyPatch,
}

#[async_trait]
impl PersonalDataProvider for RemotePaymentsPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "payments"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| {
                def.class == DataClass::Billing || def.class == DataClass::TransactionRecord
            })
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        self.call_for("/internal/privacy/collect", subject).await
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        self.call_for("/internal/privacy/erase", &EraseRequest { subject, mode })
            .await
    }

    async fn rectify(
        &self,
        subject: &SubjectRef,
        patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        if !patch.corrections.iter().any(|c| c.context == "payments") {
            return Ok(());
        }
        self.call::<_, serde::de::IgnoredAny>(
            "/internal/privacy/rectify",
            &RectifyRequest { subject, patch },
        )
        .await
        .map(|_| ())
    }
}
