//! Subject-indexed media export and asynchronous erasure through hitox.
use crate::MediaConfig;
use async_trait::async_trait;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport, LegalBasis,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetentionAction, SensitivityClass,
    SubjectRef,
};

#[derive(Default)]
pub struct MediaPrivacyProvider {
    tx: Option<TxManager>,
    config: MediaConfig,
}
impl MediaPrivacyProvider {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_storage(tx: TxManager, config: MediaConfig) -> Self {
        Self {
            tx: Some(tx),
            config,
        }
    }
    async fn assets(
        &self,
        subject: &SubjectRef,
    ) -> Result<Vec<(TenantId, serde_json::Value)>, PrivacyError> {
        let tx = self.tx.as_ref().ok_or_else(|| {
            PrivacyError::Upstream("media ownership index is unconfigured".to_string())
        })?;
        let mut scope = tx
            .begin_unscoped("media_privacy_lookup", "media")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let owners: Vec<(i64, String)> = sqlx::query_as("SELECT tenant_id, owner_ref FROM media.subject_namespaces WHERE subject_key = $1 AND ($2::bigint IS NULL OR tenant_id = $2)")
    .bind(subject.canonical_key()).bind(subject.tenant_id.map(|id| id.as_i64())).fetch_all(&mut **scope.tx_mut().as_inner()).await.map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        if owners.is_empty() {
            return Ok(Vec::new());
        }
        if self.config.base_url.is_empty() || self.config.service_api_key.is_empty() {
            return Err(PrivacyError::Upstream(
                "media service is unconfigured".to_string(),
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
        let mut assets = Vec::new();
        for (tenant, owner) in owners {
            let tenant_id =
                TenantId::try_from(tenant).map_err(|err| PrivacyError::Storage(err.to_string()))?;
            let mut after = 0_i64;
            loop {
                let response = http
                    .get(format!(
                        "{}/v1/assets",
                        self.config.base_url.trim_end_matches('/')
                    ))
                    .bearer_auth(&self.config.service_api_key)
                    .header("X-Namespace-ID", tenant.to_string())
                    .query(&[("owner_id", owner.clone()), ("after", after.to_string())])
                    .send()
                    .await
                    .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
                if !response.status().is_success() {
                    return Err(PrivacyError::Upstream(format!(
                        "media owner listing failed: {}",
                        response.status()
                    )));
                }
                let page: Vec<serde_json::Value> = response
                    .json()
                    .await
                    .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
                let count = page.len();
                for entry in page {
                    let id = entry
                        .pointer("/asset/id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| id.parse::<i64>().ok())
                        .filter(|id| *id > after)
                        .ok_or_else(|| {
                            PrivacyError::Upstream("invalid media pagination cursor".to_string())
                        })?;
                    after = id;
                    assets.push((tenant_id, entry));
                }
                if count < 100 {
                    break;
                }
            }
        }
        Ok(assets)
    }
}
#[async_trait]
impl PersonalDataProvider for MediaPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "media"
    }
    fn data_classes(&self) -> Vec<DataClassDef> {
        vec![DataClassDef {
            class: DataClass::MediaAsset,
            category: "Media",
            sensitivity: SensitivityClass::Standard,
            basis: LegalBasis::Contract,
            period_days: 1095,
            action: RetentionAction::Delete,
        }]
    }
    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        Ok(DataBundle {
            context: "media".to_string(),
            entries: self
                .assets(subject)
                .await?
                .into_iter()
                .map(|(_, value)| DataEntry {
                    data_class: DataClass::MediaAsset,
                    category: "Media".to_string(),
                    label: "Uploaded media and original download".to_string(),
                    value,
                })
                .collect(),
        })
    }
    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let assets = self.assets(subject).await?;
        if assets.is_empty() {
            return Ok(EraseReport {
                context: "media".to_string(),
                ..EraseReport::default()
            });
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
        for (tenant, asset) in &assets {
            if asset
                .pointer("/asset/status")
                .and_then(|status| status.as_str())
                == Some("deleting")
            {
                continue;
            }
            let id = asset
                .pointer("/asset/id")
                .and_then(|id| id.as_str())
                .ok_or_else(|| PrivacyError::Upstream("missing media id".to_string()))?;
            let response = http
                .delete(format!(
                    "{}/v1/assets/{}",
                    self.config.base_url.trim_end_matches('/'),
                    id
                ))
                .bearer_auth(&self.config.service_api_key)
                .header("X-Namespace-ID", tenant.to_string())
                .send()
                .await
                .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::NOT_FOUND
            {
                return Err(PrivacyError::Upstream(format!(
                    "media deletion failed: {}",
                    response.status()
                )));
            }
        }
        Err(PrivacyError::ProcessingPending(
            "media deletion is queued; waiting for object purge".to_string(),
        ))
    }
    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        Ok(())
    }
}
