use std::sync::Arc;

use crate::application::{
    CompleteAssetUpload, CreateAssetDelivery, CreateAssetUpload, DeleteMediaAsset, GetMediaAsset,
    PresignAssetParts,
};

#[derive(Clone)]
pub struct MediaState {
    pub create_asset_upload: Arc<CreateAssetUpload>,
    pub presign_asset_parts: Arc<PresignAssetParts>,
    pub complete_asset_upload: Arc<CompleteAssetUpload>,
    pub get_media_asset: Arc<GetMediaAsset>,
    pub delete_media_asset: Arc<DeleteMediaAsset>,
    pub create_asset_delivery: Arc<CreateAssetDelivery>,
}
