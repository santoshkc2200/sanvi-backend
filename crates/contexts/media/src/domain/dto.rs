use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AssetVisibility {
    #[default]
    Private,
    Public,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateAssetInput {
    pub kind: String,
    pub content_type: String,
    pub filename: String,
    pub size_bytes: i64,
    #[serde(default)]
    pub visibility: AssetVisibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UploadPartView {
    pub part_number: i64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssetUploadView {
    pub asset_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_urls: Option<Vec<UploadPartView>>,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompletedPartInput {
    pub part_number: i64,
    pub etag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssetView {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    pub kind: String,
    pub status: String,
    /// Set only on the degraded read path (TASK-023, FR-1113): object
    /// storage is down, so there is no live metadata — only this marker
    /// (`object_storage_unavailable`) for the placeholder UI. `None` on
    /// every live read, so a placeholder is never mistaken for metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    pub progress_percent: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RenditionView {
    pub name: String,
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssetDeliveryView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renditions: Vec<RenditionView>,
}
