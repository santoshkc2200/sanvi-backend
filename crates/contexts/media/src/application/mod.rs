use std::sync::Arc;

use sanvi_kernel::TenantId;

use crate::domain::{
    AssetDeliveryView, AssetUploadView, AssetView, CompletedPartInput, CreateAssetInput,
    MediaError, MediaGatewayPort, UploadPartView,
};

pub struct CreateAssetUpload {
    gateway: Arc<dyn MediaGatewayPort>,
    owner_index: Option<Arc<dyn crate::domain::MediaOwnerIndexPort>>,
}

impl CreateAssetUpload {
    pub fn new(gateway: Arc<dyn MediaGatewayPort>) -> Self {
        Self {
            gateway,
            owner_index: None,
        }
    }

    pub fn with_owner_index(mut self, index: Arc<dyn crate::domain::MediaOwnerIndexPort>) -> Self {
        self.owner_index = Some(index);
        self
    }

    #[tracing::instrument(name = "media.create_asset_upload", skip_all)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        idempotency_key: String,
        input: CreateAssetInput,
    ) -> Result<AssetUploadView, MediaError> {
        if let (Some(index), Some(owner)) = (&self.owner_index, &input.owner_id) {
            // Persist ownership before the external side effect so retries find orphaned uploads.
            index.remember(tenant_id, owner).await?;
        }
        self.gateway
            .create_asset(tenant_id, &idempotency_key, input)
            .await
    }
}

pub struct PresignAssetParts {
    gateway: Arc<dyn MediaGatewayPort>,
}

impl PresignAssetParts {
    pub fn new(gateway: Arc<dyn MediaGatewayPort>) -> Self {
        Self { gateway }
    }

    #[tracing::instrument(name = "media.presign_asset_parts", skip_all)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        asset_id: String,
        part_numbers: Vec<i64>,
    ) -> Result<Vec<UploadPartView>, MediaError> {
        self.gateway
            .presign_parts(tenant_id, &asset_id, part_numbers)
            .await
    }
}

pub struct CompleteAssetUpload {
    gateway: Arc<dyn MediaGatewayPort>,
}

impl CompleteAssetUpload {
    pub fn new(gateway: Arc<dyn MediaGatewayPort>) -> Self {
        Self { gateway }
    }

    #[tracing::instrument(name = "media.complete_asset_upload", skip_all)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        asset_id: String,
        parts: Vec<CompletedPartInput>,
    ) -> Result<AssetView, MediaError> {
        self.gateway
            .complete_asset(tenant_id, &asset_id, parts)
            .await
    }
}

pub struct GetMediaAsset {
    gateway: Arc<dyn MediaGatewayPort>,
}

impl GetMediaAsset {
    pub fn new(gateway: Arc<dyn MediaGatewayPort>) -> Self {
        Self { gateway }
    }

    /// The object-storage read path (TASK-023, FR-1113): while storage is
    /// down the gateway reports `Unavailable`, and the read degrades to a
    /// marked placeholder instead of a 500 — the course page renders its
    /// placeholder UI. `placeholder` is `Some` only here, and `status` is
    /// `degraded`, so the marker is never mistaken for live metadata.
    /// Mutations never take this path: they propagate `Unavailable` and
    /// the handler renders the honest `503 media_unavailable`.
    #[tracing::instrument(name = "media.get_media_asset", skip_all)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        asset_id: String,
    ) -> Result<AssetView, MediaError> {
        match self.gateway.get_asset(tenant_id, &asset_id).await {
            Ok(view) => Ok(view),
            Err(MediaError::Unavailable { detail }) => {
                tracing::warn!(asset_id = %asset_id, error = %detail, "object storage down; serving placeholder");
                Ok(AssetView {
                    id: asset_id,
                    owner_id: None,
                    external_ref: None,
                    kind: "unknown".to_string(),
                    status: "degraded".to_string(),
                    placeholder: Some("object_storage_unavailable".to_string()),
                    duration_ms: None,
                    width: None,
                    height: None,
                    progress_percent: 0,
                    error_message: None,
                    version: 0,
                    updated_at: None,
                })
            }
            Err(err) => Err(err),
        }
    }
}

pub struct DeleteMediaAsset {
    gateway: Arc<dyn MediaGatewayPort>,
}

impl DeleteMediaAsset {
    pub fn new(gateway: Arc<dyn MediaGatewayPort>) -> Self {
        Self { gateway }
    }

    #[tracing::instrument(name = "media.delete_media_asset", skip_all)]
    pub async fn execute(&self, tenant_id: TenantId, asset_id: String) -> Result<(), MediaError> {
        self.gateway.delete_asset(tenant_id, &asset_id).await
    }
}

pub struct CreateAssetDelivery {
    gateway: Arc<dyn MediaGatewayPort>,
}

impl CreateAssetDelivery {
    pub fn new(gateway: Arc<dyn MediaGatewayPort>) -> Self {
        Self { gateway }
    }

    #[tracing::instrument(name = "media.create_asset_delivery", skip_all)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        asset_id: String,
    ) -> Result<AssetDeliveryView, MediaError> {
        self.gateway.create_delivery(tenant_id, &asset_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    fn test_tenant() -> TenantId {
        TenantId::try_from(1_i64).unwrap()
    }

    fn test_asset_view(id: &str) -> AssetView {
        AssetView {
            id: id.to_string(),
            owner_id: None,
            external_ref: None,
            kind: "image".to_string(),
            status: "ready".to_string(),
            placeholder: None,
            duration_ms: None,
            width: None,
            height: None,
            progress_percent: 100,
            error_message: None,
            version: 1,
            updated_at: None,
        }
    }

    struct FakeGateway;

    #[async_trait]
    impl MediaGatewayPort for FakeGateway {
        async fn create_asset(
            &self,
            tenant_id: TenantId,
            idempotency_key: &str,
            input: CreateAssetInput,
        ) -> Result<AssetUploadView, MediaError> {
            assert_eq!(tenant_id, test_tenant());
            assert_eq!(idempotency_key, "idem-1");
            assert_eq!(input.filename, "a.png");
            Ok(AssetUploadView {
                asset_id: "a1".to_string(),
                upload_id: None,
                upload_url: Some("https://s3.example/put".to_string()),
                part_size: None,
                part_urls: None,
                expires_at: "2026-09-08T00:00:00Z".to_string(),
            })
        }

        async fn presign_parts(
            &self,
            tenant_id: TenantId,
            asset_id: &str,
            part_numbers: Vec<i64>,
        ) -> Result<Vec<UploadPartView>, MediaError> {
            assert_eq!(tenant_id, test_tenant());
            assert_eq!(asset_id, "a1");
            assert_eq!(part_numbers, vec![1, 2]);
            Ok(vec![
                UploadPartView {
                    part_number: 1,
                    url: "https://s3.example/part1".to_string(),
                },
                UploadPartView {
                    part_number: 2,
                    url: "https://s3.example/part2".to_string(),
                },
            ])
        }

        async fn complete_asset(
            &self,
            tenant_id: TenantId,
            asset_id: &str,
            parts: Vec<CompletedPartInput>,
        ) -> Result<AssetView, MediaError> {
            assert_eq!(tenant_id, test_tenant());
            assert_eq!(asset_id, "a1");
            assert_eq!(parts.len(), 1);
            assert_eq!(parts[0].part_number, 1);
            assert_eq!(parts[0].etag, "etag-1");
            Ok(test_asset_view("a1"))
        }

        async fn get_asset(
            &self,
            tenant_id: TenantId,
            asset_id: &str,
        ) -> Result<AssetView, MediaError> {
            assert_eq!(tenant_id, test_tenant());
            assert_eq!(asset_id, "a1");
            Ok(test_asset_view("a1"))
        }

        async fn delete_asset(
            &self,
            tenant_id: TenantId,
            asset_id: &str,
        ) -> Result<(), MediaError> {
            assert_eq!(tenant_id, test_tenant());
            assert_eq!(asset_id, "a1");
            Ok(())
        }

        async fn create_delivery(
            &self,
            tenant_id: TenantId,
            asset_id: &str,
        ) -> Result<AssetDeliveryView, MediaError> {
            assert_eq!(tenant_id, test_tenant());
            assert_eq!(asset_id, "a1");
            Ok(AssetDeliveryView {
                url: Some("https://cdn.example/a1".to_string()),
                expires_at: Some("2026-09-08T00:00:00Z".to_string()),
                renditions: vec![],
            })
        }
    }

    fn gateway() -> Arc<dyn MediaGatewayPort> {
        Arc::new(FakeGateway)
    }

    #[tokio::test]
    async fn create_asset_upload_delegates_to_the_gateway_with_the_same_arguments() {
        let use_case = CreateAssetUpload::new(gateway());
        let view = use_case
            .execute(
                test_tenant(),
                "idem-1".to_string(),
                CreateAssetInput {
                    kind: "image".to_string(),
                    content_type: "image/png".to_string(),
                    filename: "a.png".to_string(),
                    size_bytes: 10,
                    visibility: crate::domain::AssetVisibility::Private,
                    owner_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(view.asset_id, "a1");
        assert_eq!(view.upload_url.as_deref(), Some("https://s3.example/put"));
    }

    #[tokio::test]
    async fn presign_asset_parts_delegates_to_the_gateway_with_the_same_arguments() {
        let use_case = PresignAssetParts::new(gateway());
        let parts = use_case
            .execute(test_tenant(), "a1".to_string(), vec![1, 2])
            .await
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].part_number, 1);
        assert_eq!(parts[0].url, "https://s3.example/part1");
        assert_eq!(parts[1].part_number, 2);
        assert_eq!(parts[1].url, "https://s3.example/part2");
    }

    #[tokio::test]
    async fn complete_asset_upload_delegates_to_the_gateway_with_the_same_arguments() {
        let use_case = CompleteAssetUpload::new(gateway());
        let view = use_case
            .execute(
                test_tenant(),
                "a1".to_string(),
                vec![CompletedPartInput {
                    part_number: 1,
                    etag: "etag-1".to_string(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(view.id, "a1");
        assert_eq!(view.status, "ready");
    }

    #[tokio::test]
    async fn get_media_asset_delegates_to_the_gateway_with_the_same_arguments() {
        let use_case = GetMediaAsset::new(gateway());
        let view = use_case
            .execute(test_tenant(), "a1".to_string())
            .await
            .unwrap();
        assert_eq!(view.id, "a1");
        assert_eq!(view.status, "ready");
    }

    #[tokio::test]
    async fn delete_media_asset_delegates_to_the_gateway_with_the_same_arguments() {
        let use_case = DeleteMediaAsset::new(gateway());
        use_case
            .execute(test_tenant(), "a1".to_string())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_asset_delivery_delegates_to_the_gateway_with_the_same_arguments() {
        let use_case = CreateAssetDelivery::new(gateway());
        let view = use_case
            .execute(test_tenant(), "a1".to_string())
            .await
            .unwrap();
        assert_eq!(view.url.as_deref(), Some("https://cdn.example/a1"));
    }
}
