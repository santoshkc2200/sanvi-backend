//! Theme registry models: the platform-owned catalog of installable themes
//! and their immutable versions.

use chrono::{DateTime, Utc};
use sanvi_kernel::Id;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::error::ThemingError;
use crate::domain::manifest::Manifest;
use crate::domain::tokens::{TokenNode, parse_token_document};

pub enum ThemeTag {}
pub type ThemeId = Id<ThemeTag>;

pub enum ThemeVersionTag {}
pub type ThemeVersionId = Id<ThemeVersionTag>;

/// Who can see a theme in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThemeVisibility {
    /// Installed by default for every tenant.
    Public,
    /// Only visible to tenants with the `required_feature` entitlement.
    Premium,
}

impl ThemeVisibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Premium => "premium",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ThemingError> {
        match raw {
            "public" => Ok(Self::Public),
            "premium" => Ok(Self::Premium),
            other => Err(ThemingError::InvalidManifest(format!(
                "unknown theme visibility {other:?}"
            ))),
        }
    }
}

/// Lifecycle status of a theme in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThemeStatus {
    Active,
    Deprecated,
}

impl ThemeStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Deprecated => "deprecated",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ThemingError> {
        match raw {
            "active" => Ok(Self::Active),
            "deprecated" => Ok(Self::Deprecated),
            other => Err(ThemingError::InvalidManifest(format!(
                "unknown theme status {other:?}"
            ))),
        }
    }
}

/// A registry entry (`theming.themes`).
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Theme {
    #[schema(value_type = String)]
    pub id: ThemeId,
    pub key: String,
    pub publisher: String,
    pub visibility: ThemeVisibility,
    pub required_feature: Option<String>,
    pub status: ThemeStatus,
    pub created_at: DateTime<Utc>,
}

/// One immutable version of a theme (`theming.theme_versions`).
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeVersion {
    pub id: ThemeVersionId,
    pub theme_id: ThemeId,
    pub semver: Version,
    pub theme_api_range: VersionReq,
    pub manifest: Manifest,
    pub tokens: TokenModes,
    pub artifact_uri: String,
    pub artifact_digest: String,
    pub published_at: DateTime<Utc>,
    pub yanked_at: Option<DateTime<Utc>>,
}

impl ThemeVersion {
    pub fn is_yanked(&self) -> bool {
        self.yanked_at.is_some()
    }

    /// The tokens the version ships, per mode.
    pub fn token_tree(&self, dark: bool) -> Option<&TokenNode> {
        if dark {
            self.tokens.dark.as_ref()
        } else {
            Some(&self.tokens.light)
        }
    }
}

/// Token documents per mode, as stored in `theming.theme_versions.tokens`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenModes {
    pub light: TokenNode,
    pub dark: Option<TokenNode>,
}

impl TokenModes {
    /// Parse and validate the stored `{light: …, dark: …}` JSON shape.
    pub fn parse(value: &serde_json::Value) -> Result<Self, ThemingError> {
        let light = value
            .get("light")
            .ok_or_else(|| ThemingError::InvalidTokens("missing light token set".to_string()))?;
        let light = parse_token_document(light)?;
        let dark = match value.get("dark") {
            Some(dark) if !dark.is_null() => Some(parse_token_document(dark)?),
            _ => None,
        };
        Ok(Self { light, dark })
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::json!({ "light": self.light });
        if let Some(dark) = &self.dark {
            value["dark"] = serde_json::to_value(dark).expect("token nodes serialize");
        }
        value
    }
}

/// The platform's default theme: tenants with no `live` row render `base`.
pub const DEFAULT_THEME_KEY: &str = "base";
