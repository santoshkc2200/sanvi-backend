use async_trait::async_trait;
use sanvi_kernel::TenantId;

use super::dto::{
    AssetDeliveryView, AssetUploadView, AssetView, CompletedPartInput, CreateAssetInput,
    UploadPartView,
};
use super::error::MediaError;

/// Driven port to the generic media service. `tenant_id` is converted to
/// `hitox-media-service`'s opaque `X-Namespace-ID` by the implementation,
/// never by a caller — see Global Constraints.
#[async_trait]
pub trait MediaGatewayPort: Send + Sync {
    async fn create_asset(
        &self,
        tenant_id: TenantId,
        idempotency_key: &str,
        input: CreateAssetInput,
    ) -> Result<AssetUploadView, MediaError>;

    async fn presign_parts(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
        part_numbers: Vec<i64>,
    ) -> Result<Vec<UploadPartView>, MediaError>;

    async fn complete_asset(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
        parts: Vec<CompletedPartInput>,
    ) -> Result<AssetView, MediaError>;

    async fn get_asset(&self, tenant_id: TenantId, asset_id: &str)
    -> Result<AssetView, MediaError>;

    async fn delete_asset(&self, tenant_id: TenantId, asset_id: &str) -> Result<(), MediaError>;

    async fn create_delivery(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
    ) -> Result<AssetDeliveryView, MediaError>;
}

/// Durable subject ownership used to locate uploaded personal media for rights requests.
#[async_trait]
pub trait MediaOwnerIndexPort: Send + Sync {
    async fn remember(&self, tenant_id: TenantId, owner: &str) -> Result<(), MediaError>;
}
