//! List the themes a tenant may choose from, filtered by entitlements
//! (premium themes are invisible without the feature) and by withdrawal
//! (yanked versions are never offered).

use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::PgPool;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::views::AvailableThemeView;
use crate::domain::error::ThemingError;
use crate::domain::theme::{ThemeVersion, ThemeVisibility};
use crate::domain::{Theme, ThemeFeatureGate, ThemeRepository};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AvailableThemesView {
    pub themes: Vec<AvailableThemeView>,
}

/// `GET /api/v1/tenant/themes/available` — the entitlement-filtered catalog.
pub struct ListAvailableThemes {
    themes: Arc<dyn ThemeRepository>,
    gate: Arc<dyn ThemeFeatureGate>,
    pool: PgPool,
    asset_base_url: String,
}

impl ListAvailableThemes {
    pub fn new(
        themes: Arc<dyn ThemeRepository>,
        gate: Arc<dyn ThemeFeatureGate>,
        pool: PgPool,
        asset_base_url: String,
    ) -> Self {
        Self {
            themes,
            gate,
            pool,
            asset_base_url,
        }
    }

    #[tracing::instrument(name = "theming.list_available_themes", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(&self, tenant_id: TenantId) -> Result<AvailableThemesView, ThemingError> {
        let registry = self.themes.list(&self.pool).await?;
        let mut views = Vec::new();
        for theme in registry {
            // Premium themes are invisible to tenants without the feature.
            if theme.visibility == ThemeVisibility::Premium {
                let required = theme.required_feature.clone().ok_or_else(|| {
                    ThemingError::InvalidManifest(format!(
                        "premium theme {} has no required_feature",
                        theme.key
                    ))
                })?;
                if !self.gate.is_enabled(tenant_id, &required).await? {
                    continue;
                }
            }
            let Some(version) = self
                .themes
                .get_latest_version(&self.pool, &theme.key)
                .await?
            else {
                // Nothing published yet: not selectable.
                continue;
            };
            views.push(to_view(&theme, &version, &self.asset_base_url));
        }
        views.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(AvailableThemesView { themes: views })
    }
}

fn to_view(theme: &Theme, version: &ThemeVersion, asset_base_url: &str) -> AvailableThemeView {
    let preview_url = version.manifest.assets.preview.as_ref().map(|path| {
        format!(
            "{asset_base_url}/themes/{}/{}/{}",
            theme.key, version.semver, path
        )
    });
    AvailableThemeView {
        key: theme.key.clone(),
        name: version.manifest.name.clone(),
        version: version.semver.to_string(),
        capabilities: version.manifest.capabilities.clone(),
        visibility: theme.visibility,
        required_feature: theme.required_feature.clone(),
        status: theme.status,
        dark_mode: version.manifest.has_dark_mode(),
        preview_url,
    }
}
