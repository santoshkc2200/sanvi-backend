//! Tenant theme state: the per-tenant draft/live rows, overrides, brand
//! assets and the publish snapshots rollback restores.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use semver::Version;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::error::ThemingError;
use crate::domain::manifest::Manifest;
use crate::domain::theme::ThemeId;
use crate::domain::tokens::TokenValue;

/// Draft or live — the two rows a tenant may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThemeState {
    Draft,
    Live,
}

impl ThemeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Live => "live",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ThemingError> {
        match raw {
            "draft" => Ok(Self::Draft),
            "live" => Ok(Self::Live),
            other => Err(ThemingError::InvalidState(format!(
                "unknown theme state {other:?}"
            ))),
        }
    }
}

/// A tenant's layout adjustment: which slots to hide on an unlocked layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LayoutOverride {
    pub hidden_slots: Vec<String>,
}

/// The brand asset kinds a tenant can upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrandAssetKind {
    Logo,
    Favicon,
    OgImage,
}

impl BrandAssetKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Logo => "logo",
            Self::Favicon => "favicon",
            Self::OgImage => "og_image",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ThemingError> {
        match raw {
            "logo" => Ok(Self::Logo),
            "favicon" => Ok(Self::Favicon),
            "og_image" => Ok(Self::OgImage),
            other => Err(ThemingError::InvalidBrandAsset(format!(
                "unknown asset kind {other:?} (logo | favicon | og_image)"
            ))),
        }
    }
}

/// One validated, stored brand asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BrandAsset {
    pub kind: BrandAssetKind,
    /// The sha256 digest of the stored bytes (content-addressed file name).
    pub digest: String,
    pub mime: String,
    /// The stored extension, e.g. `png`.
    pub ext: String,
    pub uploaded_at: DateTime<Utc>,
}

/// The tenant's brand assets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BrandAssets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<BrandAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<BrandAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub og_image: Option<BrandAsset>,
}

impl BrandAssets {
    pub fn get(&self, kind: BrandAssetKind) -> Option<&BrandAsset> {
        match kind {
            BrandAssetKind::Logo => self.logo.as_ref(),
            BrandAssetKind::Favicon => self.favicon.as_ref(),
            BrandAssetKind::OgImage => self.og_image.as_ref(),
        }
    }

    pub fn set(&mut self, asset: BrandAsset) {
        match asset.kind {
            BrandAssetKind::Logo => self.logo = Some(asset),
            BrandAssetKind::Favicon => self.favicon = Some(asset),
            BrandAssetKind::OgImage => self.og_image = Some(asset),
        }
    }
}

/// A tenant theme row (draft or live).
#[derive(Debug, Clone, PartialEq)]
pub struct TenantTheme {
    pub tenant_id: TenantId,
    pub state: ThemeState,
    pub theme_id: ThemeId,
    pub theme_key: String,
    pub semver: Version,
    pub token_overrides: BTreeMap<String, TokenValue>,
    pub layout_overrides: BTreeMap<String, LayoutOverride>,
    pub assets: BrandAssets,
    pub custom_css: Option<String>,
    pub revision: i32,
    pub updated_by: Option<UserId>,
    pub updated_at: DateTime<Utc>,
}

impl TenantTheme {
    /// The tenant's default draft: the base theme, pinned to its latest
    /// published version, no overrides.
    pub fn default_draft(
        tenant_id: TenantId,
        theme_id: ThemeId,
        theme_key: String,
        version: Version,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            tenant_id,
            state: ThemeState::Draft,
            theme_id,
            theme_key,
            semver: version,
            token_overrides: BTreeMap::new(),
            layout_overrides: BTreeMap::new(),
            assets: BrandAssets::default(),
            custom_css: None,
            revision: 0,
            updated_by: None,
            updated_at: now,
        }
    }
}

/// The publish snapshot (immutable revision) rollback restores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantThemeSnapshot {
    pub theme_id: ThemeId,
    pub theme_key: String,
    pub semver: Version,
    pub token_overrides: BTreeMap<String, TokenValue>,
    pub layout_overrides: BTreeMap<String, LayoutOverride>,
    pub assets: BrandAssets,
    pub custom_css: Option<String>,
}

impl TenantThemeSnapshot {
    pub fn of(theme: &TenantTheme) -> Self {
        Self {
            theme_id: theme.theme_id,
            theme_key: theme.theme_key.clone(),
            semver: theme.semver.clone(),
            token_overrides: theme.token_overrides.clone(),
            layout_overrides: theme.layout_overrides.clone(),
            assets: theme.assets.clone(),
            custom_css: theme.custom_css.clone(),
        }
    }

    /// Apply the snapshot back onto a row (rollback).
    pub fn apply(&self, theme: &mut TenantTheme) {
        theme.theme_id = self.theme_id;
        theme.theme_key = self.theme_key.clone();
        theme.semver = self.semver.clone();
        theme.token_overrides = self.token_overrides.clone();
        theme.layout_overrides = self.layout_overrides.clone();
        theme.assets = self.assets.clone();
        theme.custom_css = self.custom_css.clone();
    }
}

/// A tenant's layout overrides must only touch layouts the manifest defines,
/// must respect `locked`, and every hidden slot must exist in the layout.
pub fn validate_layout_overrides(
    overrides: &BTreeMap<String, LayoutOverride>,
    manifest: &Manifest,
) -> Result<(), ThemingError> {
    for (layout, adjustment) in overrides {
        let Some(spec) = manifest.layouts.get(layout) else {
            return Err(ThemingError::LayoutOverrideNotAllowed(format!(
                "layout {layout:?} is not defined by this theme"
            )));
        };
        if spec.locked {
            return Err(ThemingError::LayoutOverrideNotAllowed(format!(
                "layout {layout:?} is locked by this theme"
            )));
        }
        for slot in &adjustment.hidden_slots {
            if !spec.slots.contains(slot) {
                return Err(ThemingError::LayoutOverrideNotAllowed(format!(
                    "slot {slot:?} does not exist in layout {layout:?}"
                )));
            }
        }
    }
    Ok(())
}
