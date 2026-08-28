//! The resolved theme: the single document the frontend consumes for SSR —
//! fully merged tokens, a precompiled CSS custom-property block, layouts,
//! fonts and asset URLs. One request, one cache key, instant swap.

use std::collections::BTreeMap;

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::domain::manifest::FontSpec;
use crate::domain::tenant_theme::{BrandAsset, BrandAssets, LayoutOverride};
use crate::domain::tokens::TokenNode;

/// A layout after tenant adjustments (hidden slots removed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ResolvedLayout {
    pub slots: Vec<String>,
}

impl ResolvedLayout {
    pub fn from_slots(slots: &[String], adjustment: Option<&LayoutOverride>) -> Self {
        let hidden = adjustment
            .map(|value| value.hidden_slots.as_slice())
            .unwrap_or(&[]);
        Self {
            slots: slots
                .iter()
                .filter(|slot| !hidden.contains(slot))
                .cloned()
                .collect(),
        }
    }
}

/// Absolute URLs for a tenant's brand assets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct BrandAssetUrls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub og_image: Option<String>,
}

impl BrandAssetUrls {
    pub fn resolve(assets: &BrandAssets, base_url: &str, tenant_id: sanvi_kernel::TenantId) -> Self {
        let url = |asset: Option<&BrandAsset>| {
            asset.map(|asset| {
                format!(
                    "{base_url}/brand/{tenant_id}/{}.{}",
                    asset.digest, asset.ext
                )
            })
        };
        Self {
            logo: url(assets.logo.as_ref()),
            favicon: url(assets.favicon.as_ref()),
            og_image: url(assets.og_image.as_ref()),
        }
    }
}

/// Theme-provided reference assets (preview/screenshots), resolved to URLs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ThemeAssetUrls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<String>,
}

impl ThemeAssetUrls {
    pub fn resolve(
        theme_key: &str,
        version: &Version,
        preview: Option<&String>,
        screenshots: &[String],
        base_url: &str,
    ) -> Self {
        let artifact = format!("{base_url}/themes/{theme_key}/{version}");
        Self {
            preview: preview.map(|path| format!("{artifact}/{path}")),
            screenshots: screenshots
                .iter()
                .map(|path| format!("{artifact}/{path}"))
                .collect(),
        }
    }
}

/// The fully resolved theme document, ready for the frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ResolvedTheme {
    pub theme_key: String,
    pub theme_version: String,
    pub theme_api: String,
    pub capabilities: Vec<String>,
    /// The merged nested token tree (light mode).
    pub tokens: Value,
    /// The merged dark token tree, when the theme ships dark mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dark_tokens: Option<Value>,
    /// Precompiled `:root{--…}` block (light + dark media query).
    pub css_vars: String,
    pub layouts: BTreeMap<String, ResolvedLayout>,
    pub fonts: Vec<FontSpec>,
    pub theme_assets: ThemeAssetUrls,
    pub brand_assets: BrandAssetUrls,
    /// Sanitised tenant CSS, scoped under the storefront wrapper class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_css: Option<String>,
    /// The live revision this document reflects (draft previews: 0).
    pub revision: i32,
    pub locale: String,
    /// ETag: hash of (theme, version, revision, locale, api).
    pub etag: String,
}

/// The ETag for a resolution: everything that changes the rendered output.
pub fn resolution_etag(theme_key: &str, version: &str, revision: i32, locale: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(theme_key.as_bytes());
    hasher.update(b"|");
    hasher.update(version.as_bytes());
    hasher.update(b"|");
    hasher.update(revision.to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(locale.as_bytes());
    hasher.update(b"|");
    hasher.update(crate::domain::manifest::THEME_API_VERSION.as_bytes());
    format!("\"{}\"", hex::encode(hasher.finalize()))
}

/// The scope class custom CSS is wrapped under on a storefront.
pub const CUSTOM_CSS_SCOPE_CLASS: &str = ".sanvi-theme-scope";

/// Merge a token tree into a serializable value (light + optional dark).
pub fn merged_token_value(light: &TokenNode, dark: Option<&TokenNode>) -> (Value, Option<Value>) {
    let light_value = serde_json::to_value(light).expect("token trees serialize");
    let dark_value = dark.map(|dark| serde_json::to_value(dark).expect("token trees serialize"));
    (light_value, dark_value)
}
