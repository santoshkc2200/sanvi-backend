//! API views (the application layer's output types).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::manifest::{FontSpec, LocalizedName};
use crate::domain::tenant_theme::{BrandAssets, LayoutOverride, ThemeState};
use crate::domain::theme::{Theme, ThemeStatus, ThemeVersion, ThemeVisibility};
use crate::domain::tokens::TokenValue;

/// A theme in the tenant-facing catalog (entitlement-filtered).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AvailableThemeView {
    pub key: String,
    pub name: LocalizedName,
    pub version: String,
    pub capabilities: Vec<String>,
    pub visibility: ThemeVisibility,
    pub required_feature: Option<String>,
    pub status: ThemeStatus,
    pub dark_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
}

/// The tenant's draft (or live) theme state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TenantThemeView {
    pub state: ThemeState,
    pub theme_key: String,
    pub version: String,
    pub token_overrides: BTreeMap<String, TokenValue>,
    pub layout_overrides: BTreeMap<String, LayoutOverride>,
    pub assets: BrandAssets,
    pub custom_css: Option<String>,
    pub revision: i32,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Theme reference data surfaced to the tenant UI (fonts/layouts the theme
/// ships, so the editor knows what is overridable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ThemeSpecView {
    pub key: String,
    pub version: String,
    pub overridable: Vec<String>,
    pub layouts: BTreeMap<String, crate::domain::manifest::LayoutSpec>,
    pub fonts: Vec<FontSpec>,
    pub capabilities: Vec<String>,
}

/// A registry entry for the platform control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ThemeAdminView {
    pub id: uuid::Uuid,
    pub key: String,
    pub publisher: String,
    pub visibility: ThemeVisibility,
    pub required_feature: Option<String>,
    pub status: ThemeStatus,
    pub versions: Vec<ThemeVersionAdminView>,
}

/// A version row for the platform control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ThemeVersionAdminView {
    pub semver: String,
    pub theme_api_range: String,
    pub artifact_uri: String,
    pub artifact_digest: String,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub yanked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<&ThemeVersion> for ThemeVersionAdminView {
    fn from(version: &ThemeVersion) -> Self {
        Self {
            semver: version.semver.to_string(),
            theme_api_range: version.theme_api_range.to_string(),
            artifact_uri: version.artifact_uri.clone(),
            artifact_digest: version.artifact_digest.clone(),
            published_at: version.published_at,
            yanked_at: version.yanked_at,
        }
    }
}

impl ThemeAdminView {
    pub fn from_theme(theme: &Theme, versions: &[ThemeVersion]) -> Self {
        Self {
            id: theme.id,
            key: theme.key.clone(),
            publisher: theme.publisher.clone(),
            visibility: theme.visibility,
            required_feature: theme.required_feature.clone(),
            status: theme.status,
            versions: versions.iter().map(ThemeVersionAdminView::from).collect(),
        }
    }
}
