//! The product-access context's `PersonalDataProvider`.
//! Personal data recorded by product services (such as ecommerce-service's
//! `created_by` user attribution) is exported and anonymised through the service's
//! internal privacy endpoints.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use reqwest::{Client, Url};
use sanvi_kernel::UserId;
use sanvi_privacy::domain::{
    DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport, LegalBasis,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetentionAction, SensitivityClass,
    SubjectRef,
};
use serde::Deserialize;

use crate::config::ProductAccessConfig;
use crate::domain::{ECOMMERCE, PrivacyClaims, TokenSigner};

struct ProviderInner {
    client: Client,
    base_url: Url,
    service_key: String,
    signer: Arc<dyn TokenSigner>,
}

#[derive(Deserialize)]
struct RecordsPage {
    #[serde(default)]
    records: Vec<serde_json::Value>,
    #[serde(default)]
    next_after: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ErasureResponse {
    #[serde(default)]
    anonymised: u64,
}

#[derive(Clone)]
pub struct ProductAccessPrivacyProvider {
    inner: Option<Arc<ProviderInner>>,
}

impl Default for ProductAccessPrivacyProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ProductAccessPrivacyProvider {
    pub fn new() -> Self {
        Self::from_config(&ProductAccessConfig::default())
            .expect("default product access config is valid")
    }

    /// Constructs the provider from product-access config.
    /// An empty ecommerce `base_url` builds an unconfigured provider, which
    /// returns empty bundles and reports (ecommerce is not deployed in that
    /// environment) and logs `warn` once at construction.
    pub fn from_config(config: &ProductAccessConfig) -> anyhow::Result<Self> {
        if config.ecommerce.base_url.trim().is_empty() {
            tracing::warn!(
                "ecommerce base_url is unconfigured; product_access privacy operations will return empty results"
            );
            return Ok(Self { inner: None });
        }

        let base_url = Url::parse(config.ecommerce.base_url.trim_end_matches('/'))?;
        if !matches!(base_url.scheme(), "http" | "https") {
            anyhow::bail!("ecommerce base_url must be http(s)");
        }

        let client = Client::builder()
            .timeout(crate::adapters::upstream::TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let signer = Arc::new(crate::adapters::Ed25519Signer::from_seed_hex(
            &config.signing_key_id,
            &config.signing_key_seed,
        )?);

        Ok(Self {
            inner: Some(Arc::new(ProviderInner {
                client,
                base_url,
                service_key: config.ecommerce.service_key.clone(),
                signer,
            })),
        })
    }

    fn user_id(subject: &SubjectRef) -> Option<UserId> {
        subject
            .identifiers
            .iter()
            .find(|i| i.id_type == "user_id")
            .and_then(|i| i.value.parse::<i64>().ok())
            .and_then(|id| UserId::try_from(id).ok())
    }
}

#[async_trait]
impl PersonalDataProvider for ProductAccessPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "product_access"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        vec![DataClassDef {
            class: DataClass::EcommerceAttribution,
            category: "Ecommerce",
            sensitivity: SensitivityClass::Standard,
            basis: LegalBasis::Contract,
            period_days: 2555,
            action: RetentionAction::Anonymise,
        }]
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let user_id = match Self::user_id(subject) {
            Some(id) => id,
            None => {
                return Ok(DataBundle {
                    context: "product_access".to_string(),
                    entries: Vec::new(),
                });
            }
        };

        let inner = match self.inner.as_ref() {
            Some(inner) => inner,
            None => {
                return Ok(DataBundle {
                    context: "product_access".to_string(),
                    entries: Vec::new(),
                });
            }
        };

        let mut entries = Vec::new();
        let mut after: Option<String> = None;

        loop {
            let jti = uuid::Uuid::now_v7().simple().to_string();
            let claims = PrivacyClaims::for_subject(&ECOMMERCE, user_id, jti, Utc::now());
            let token = inner
                .signer
                .sign_privacy(&claims)
                .map_err(|err| PrivacyError::Upstream(err.to_string()))?;

            let url = format!(
                "{}/internal/v1/privacy/users/{user_id}/records",
                inner.base_url.as_str().trim_end_matches('/')
            );
            let mut req = inner
                .client
                .get(&url)
                .bearer_auth(&inner.service_key)
                .header(crate::adapters::upstream::IDENTITY_HEADER, &token)
                .query(&[("limit", "500")]);
            if let Some(ref after_id) = after {
                req = req.query(&[("after", after_id)]);
            }

            let response = req
                .send()
                .await
                .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
            if !response.status().is_success() {
                return Err(PrivacyError::Upstream(format!(
                    "ecommerce records query failed: {}",
                    response.status()
                )));
            }

            let page: RecordsPage = response
                .json()
                .await
                .map_err(|err| PrivacyError::Upstream(err.to_string()))?;

            for record in page.records {
                entries.push(DataEntry {
                    data_class: DataClass::EcommerceAttribution,
                    category: "Ecommerce".to_string(),
                    label: "Store or product you created".to_string(),
                    value: record,
                });
            }

            match page.next_after {
                Some(serde_json::Value::String(next)) if !next.is_empty() => {
                    after = Some(next);
                }
                Some(serde_json::Value::Number(next)) => {
                    after = Some(next.to_string());
                }
                _ => break,
            }
        }

        Ok(DataBundle {
            context: "product_access".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let user_id = match Self::user_id(subject) {
            Some(id) => id,
            None => {
                return Ok(EraseReport {
                    context: "product_access".to_string(),
                    erased: 0,
                    anonymised: 0,
                    retained: Vec::new(),
                });
            }
        };

        let inner = match self.inner.as_ref() {
            Some(inner) => inner,
            None => {
                return Ok(EraseReport {
                    context: "product_access".to_string(),
                    erased: 0,
                    anonymised: 0,
                    retained: Vec::new(),
                });
            }
        };

        let jti = uuid::Uuid::now_v7().simple().to_string();
        let claims = PrivacyClaims::for_subject(&ECOMMERCE, user_id, jti, Utc::now());
        let token = inner
            .signer
            .sign_privacy(&claims)
            .map_err(|err| PrivacyError::Upstream(err.to_string()))?;

        let url = format!(
            "{}/internal/v1/privacy/users/{user_id}/erasure",
            inner.base_url.as_str().trim_end_matches('/')
        );
        let response = inner
            .client
            .post(&url)
            .bearer_auth(&inner.service_key)
            .header(crate::adapters::upstream::IDENTITY_HEADER, &token)
            .send()
            .await
            .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
        if !response.status().is_success() {
            return Err(PrivacyError::Upstream(format!(
                "ecommerce erasure failed: {}",
                response.status()
            )));
        }

        let body: ErasureResponse = response
            .json()
            .await
            .map_err(|err| PrivacyError::Upstream(err.to_string()))?;

        Ok(EraseReport {
            context: "product_access".to_string(),
            erased: 0,
            anonymised: body.anonymised as usize,
            retained: Vec::new(),
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
