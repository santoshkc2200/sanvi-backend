//! Theming domain events.
//!
//! Every tenant-visible event carries the acting locale via
//! `EventEnvelope::with_locale` (kernel contract: workers never re-negotiate
//! a locale from a missing request). Events are published from the same
//! transaction as the state change that caused them.

use sanvi_kernel::events::EventEnvelope;
use serde::Serialize;
use uuid::Uuid;

pub const THEME_AGGREGATE: &str = "theming.theme";
pub const TENANT_THEME_AGGREGATE: &str = "theming.tenant_theme";
pub const BRAND_ASSET_AGGREGATE: &str = "theming.brand_asset";

pub const DEFAULT_LOCALE: &str = "en";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemingEventName {
    ThemePublished,
    ThemeYanked,
    TenantThemeChanged,
    TenantThemePublished,
    TenantThemeRolledBack,
    BrandAssetUpdated,
}

impl ThemingEventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThemePublished => "theming.theme_published",
            Self::ThemeYanked => "theming.theme_yanked",
            Self::TenantThemeChanged => "theming.tenant_theme_changed",
            Self::TenantThemePublished => "theming.tenant_theme_published",
            Self::TenantThemeRolledBack => "theming.tenant_theme_rolled_back",
            Self::BrandAssetUpdated => "theming.brand_asset_updated",
        }
    }
}

/// The events the theming context publishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThemingEvent {
    /// A new theme version was installed (artifact published).
    ThemePublished { theme_key: String, version: String },
    /// A theme version was withdrawn (kill switch; tenants stay pinned).
    ThemeYanked { theme_key: String, version: String },
    /// A tenant edited its draft (overrides, assets, custom CSS, pin).
    TenantThemeChanged {
        tenant_id: Uuid,
        /// The locale the change was made under.
        locale: String,
    },
    /// A tenant published its draft: the live theme changed. Consumers
    /// invalidate cached resolved themes.
    TenantThemePublished {
        tenant_id: Uuid,
        theme_key: String,
        version: String,
        revision: i32,
        locale: String,
    },
    /// A tenant rolled back to a previous snapshot.
    TenantThemeRolledBack {
        tenant_id: Uuid,
        revision: i32,
        locale: String,
    },
    /// A brand asset (logo/favicon/OG image) was uploaded or removed.
    BrandAssetUpdated {
        tenant_id: Uuid,
        asset_kind: String,
        locale: String,
    },
}

impl ThemingEvent {
    pub fn name(&self) -> ThemingEventName {
        match self {
            Self::ThemePublished { .. } => ThemingEventName::ThemePublished,
            Self::ThemeYanked { .. } => ThemingEventName::ThemeYanked,
            Self::TenantThemeChanged { .. } => ThemingEventName::TenantThemeChanged,
            Self::TenantThemePublished { .. } => ThemingEventName::TenantThemePublished,
            Self::TenantThemeRolledBack { .. } => ThemingEventName::TenantThemeRolledBack,
            Self::BrandAssetUpdated { .. } => ThemingEventName::BrandAssetUpdated,
        }
    }

    pub fn aggregate_id(&self) -> Uuid {
        match self {
            Self::ThemePublished { .. } | Self::ThemeYanked { .. } => Uuid::nil(),
            Self::TenantThemeChanged { tenant_id, .. }
            | Self::TenantThemePublished { tenant_id, .. }
            | Self::TenantThemeRolledBack { tenant_id, .. }
            | Self::BrandAssetUpdated { tenant_id, .. } => *tenant_id,
        }
    }

    pub fn envelope(&self) -> EventEnvelope {
        let aggregate = match self {
            Self::ThemePublished { .. } | Self::ThemeYanked { .. } => THEME_AGGREGATE,
            Self::TenantThemeChanged { .. }
            | Self::TenantThemePublished { .. }
            | Self::TenantThemeRolledBack { .. } => TENANT_THEME_AGGREGATE,
            Self::BrandAssetUpdated { .. } => BRAND_ASSET_AGGREGATE,
        };
        EventEnvelope::new(
            aggregate,
            self.aggregate_id(),
            self.name().as_str(),
            serde_json::to_value(self).expect("theming events serialize"),
        )
        .with_metadata(
            "tenant_id",
            serde_json::json!(match self {
                Self::ThemePublished { .. } | Self::ThemeYanked { .. } => "platform".to_string(),
                Self::TenantThemeChanged { tenant_id, .. }
                | Self::TenantThemePublished { tenant_id, .. }
                | Self::TenantThemeRolledBack { tenant_id, .. }
                | Self::BrandAssetUpdated { tenant_id, .. } => tenant_id.to_string(),
            }),
        )
        .with_locale(match self {
            Self::ThemePublished { .. } | Self::ThemeYanked { .. } => DEFAULT_LOCALE.to_string(),
            Self::TenantThemeChanged { locale, .. }
            | Self::TenantThemePublished { locale, .. }
            | Self::TenantThemeRolledBack { locale, .. }
            | Self::BrandAssetUpdated { locale, .. } => locale.clone(),
        })
    }
}
