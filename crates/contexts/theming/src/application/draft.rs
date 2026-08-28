//! The draft workflow: read the draft (defaulting to the base theme) and
//! edit it — theme selection/pinning, token overrides, layout adjustments,
//! custom CSS — with every change validated against the target version.

use std::collections::BTreeMap;
use std::sync::Arc;

use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::PgPool;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::views::{TenantThemeView, ThemeSpecView};
use crate::domain::error::ThemingError;
use crate::domain::sanitizer::sanitize_css;
use crate::domain::tenant_theme::{
    LayoutOverride, TenantTheme, ThemeState, validate_layout_overrides,
};
use crate::domain::theme::{
    DEFAULT_THEME_KEY, ThemeId, ThemeStatus, ThemeVersion, ThemeVisibility,
};
use crate::domain::tokens::{TokenNode, TokenValue, apply_overrides, merge_tokens};
use crate::domain::{
    Clock, EventPublisher, TenantThemeRepository, ThemeFeatureGate, ThemeRepository, ThemingEvent,
};

/// The `theming.custom_css` feature key (access catalog, phase 03).
pub const CUSTOM_CSS_FEATURE: &str = "theming.custom_css";
/// The `theming.custom_templates` feature key (access catalog, phase 03).
pub const CUSTOM_TEMPLATES_FEATURE: &str = "theming.custom_templates";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TenantThemeDraftView {
    pub theme: TenantThemeView,
    pub spec: ThemeSpecView,
}

/// The edit command. Every field is optional: `None` leaves it unchanged;
/// `custom_css: Some(None)` removes custom CSS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UpdateTenantThemeDraftCommand {
    /// Switch theme. When present with no `version`, pins the theme's
    /// latest published version.
    #[serde(default)]
    pub theme_key: Option<String>,
    /// Explicit version pin (a theme upgrade is never implicit).
    #[serde(default)]
    pub version: Option<String>,
    /// Flat token overrides (`color.brand.primary` → `{$value, $type}`).
    #[serde(default)]
    pub token_overrides: Option<BTreeMap<String, TokenValue>>,
    /// Layout adjustments (hidden slots on unlocked layouts).
    #[serde(default)]
    pub layout_overrides: Option<BTreeMap<String, LayoutOverride>>,
    /// Replacement custom CSS (`Some(None)` clears it).
    #[serde(
        default,
        deserialize_with = "deserialize_optional_nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub custom_css: Option<Option<String>>,
}

fn deserialize_optional_nullable<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

/// Validates a draft's selection and adjustments against a target version.
/// Shared by the update, publish and preview paths so a draft can never
/// hold state that fails at publish time.
pub struct DraftValidator<'a> {
    themes: &'a Arc<dyn ThemeRepository>,
    gate: &'a Arc<dyn ThemeFeatureGate>,
    pool: &'a PgPool,
    custom_css_max_bytes: usize,
}

impl<'a> DraftValidator<'a> {
    pub fn new(
        themes: &'a Arc<dyn ThemeRepository>,
        gate: &'a Arc<dyn ThemeFeatureGate>,
        pool: &'a PgPool,
        custom_css_max_bytes: usize,
    ) -> Self {
        Self {
            themes,
            gate,
            pool,
            custom_css_max_bytes,
        }
    }

    /// Resolve the target theme + version for a command, defaulting to the
    /// current selection.
    pub async fn target_version(
        &self,
        tenant_id: TenantId,
        current: &TenantTheme,
        command: &UpdateTenantThemeDraftCommand,
    ) -> Result<(ThemeId, ThemeVersion), ThemingError> {
        let theme_key = command
            .theme_key
            .clone()
            .unwrap_or_else(|| current.theme_key.clone());
        let theme = self
            .themes
            .get_by_key(self.pool, &theme_key)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(theme_key.clone()))?;

        let version = match &command.version {
            Some(semver) => self
                .themes
                .get_version(self.pool, &theme_key, semver)
                .await?
                .ok_or_else(|| ThemingError::UnknownThemeVersion {
                    theme: theme_key.clone(),
                    version: semver.clone(),
                })?,
            None if command.theme_key.is_some() => self
                .themes
                .get_latest_version(self.pool, &theme_key)
                .await?
                .ok_or_else(|| ThemingError::UnknownTheme(theme_key.clone()))?,
            None => self
                .themes
                .get_version(self.pool, &current.theme_key, &current.semver.to_string())
                .await?
                .ok_or_else(|| ThemingError::UnknownThemeVersion {
                    theme: current.theme_key.clone(),
                    version: current.semver.to_string(),
                })?,
        };

        let switching = command.theme_key.is_some() || version.semver != current.semver;
        if switching {
            if theme.status == ThemeStatus::Deprecated {
                return Err(ThemingError::ThemeDeprecated(theme_key.clone()));
            }
            if version.is_yanked() {
                return Err(ThemingError::ThemeVersionYanked {
                    theme: theme_key.clone(),
                    version: version.semver.to_string(),
                });
            }
            if theme.visibility == ThemeVisibility::Premium {
                let required = theme.required_feature.clone().ok_or_else(|| {
                    ThemingError::InvalidManifest(format!(
                        "premium theme {theme_key} has no required_feature"
                    ))
                })?;
                if !self.gate.is_enabled(tenant_id, &required).await? {
                    return Err(ThemingError::ThemeNotEntitled { feature: required });
                }
            }
        }
        Ok((theme.id, version))
    }

    /// Validate the command's overrides, layout adjustments and custom CSS
    /// against the target version. Returns the sanitised CSS (or None) and
    /// the merged tokens with overrides applied (for publish-time contrast
    /// checks and preview rendering).
    pub async fn validate_adjustments(
        &self,
        tenant_id: TenantId,
        version: &ThemeVersion,
        token_overrides: &BTreeMap<String, TokenValue>,
        layout_overrides: &BTreeMap<String, LayoutOverride>,
        custom_css: Option<&str>,
    ) -> Result<ValidatedDraft, ThemingError> {
        validate_layout_overrides(layout_overrides, &version.manifest)?;
        if !layout_overrides.is_empty()
            && !self
                .gate
                .is_enabled(tenant_id, CUSTOM_TEMPLATES_FEATURE)
                .await?
        {
            return Err(ThemingError::LayoutOverrideNotEntitled(
                CUSTOM_TEMPLATES_FEATURE.to_string(),
            ));
        }

        let css = match custom_css {
            Some(css) if !css.trim().is_empty() => {
                if !self.gate.is_enabled(tenant_id, CUSTOM_CSS_FEATURE).await? {
                    return Err(ThemingError::CustomCssNotEntitled(
                        CUSTOM_CSS_FEATURE.to_string(),
                    ));
                }
                Some(
                    sanitize_css(css, self.custom_css_max_bytes)?
                        .as_str()
                        .to_string(),
                )
            }
            _ => None,
        };

        // Merged tokens: platform base defaults ← theme tokens ← overrides.
        let base_tokens = self
            .themes
            .get_latest_version(self.pool, DEFAULT_THEME_KEY)
            .await?;
        let base_light = base_tokens
            .as_ref()
            .map(|base| &base.tokens.light)
            .unwrap_or(&version.tokens.light);
        let base_dark = base_tokens
            .as_ref()
            .and_then(|base| base.tokens.dark.as_ref());

        let merged_light = apply_overrides(
            &merge_tokens(base_light, &version.tokens.light),
            token_overrides,
            &version.manifest,
        )?;
        let merged_dark = version.tokens.dark.as_ref().map(|dark| {
            let base_dark = base_dark.unwrap_or(dark);
            apply_overrides(
                &merge_tokens(base_dark, dark),
                token_overrides,
                &version.manifest,
            )
        });
        let merged_dark = merged_dark.transpose()?;

        Ok(ValidatedDraft {
            tokens_light: merged_light,
            tokens_dark: merged_dark,
            custom_css: css,
        })
    }
}

/// The result of validating a draft's adjustments.
pub struct ValidatedDraft {
    pub tokens_light: TokenNode,
    pub tokens_dark: Option<TokenNode>,
    pub custom_css: Option<String>,
}

/// `GET /api/v1/tenant/theme/draft`.
pub struct GetTenantThemeDraft {
    themes: Arc<dyn ThemeRepository>,
    tenants: Arc<dyn TenantThemeRepository>,
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl GetTenantThemeDraft {
    pub fn new(
        themes: Arc<dyn ThemeRepository>,
        tenants: Arc<dyn TenantThemeRepository>,
        pool: PgPool,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            themes,
            tenants,
            pool,
            clock,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<TenantThemeDraftView, ThemingError> {
        let theme = match self
            .tenants
            .get(scope, tenant_id, ThemeState::Draft)
            .await?
        {
            Some(draft) => draft,
            None => self.default_draft(tenant_id).await?,
        };
        let version = self
            .themes
            .get_version(&self.pool, &theme.theme_key, &theme.semver.to_string())
            .await?
            .ok_or_else(|| ThemingError::UnknownThemeVersion {
                theme: theme.theme_key.clone(),
                version: theme.semver.to_string(),
            })?;
        Ok(TenantThemeDraftView {
            theme: to_view(&theme),
            spec: ThemeSpecView {
                key: theme.theme_key,
                version: theme.semver.to_string(),
                overridable: version.manifest.overridable.clone(),
                layouts: version.manifest.layouts.clone(),
                fonts: version.manifest.fonts.clone(),
                capabilities: version.manifest.capabilities.clone(),
            },
        })
    }

    /// The default draft: the base theme at its latest published version.
    async fn default_draft(&self, tenant_id: TenantId) -> Result<TenantTheme, ThemingError> {
        let theme = self
            .themes
            .get_by_key(&self.pool, DEFAULT_THEME_KEY)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(DEFAULT_THEME_KEY.to_string()))?;
        let version = self
            .themes
            .get_latest_version(&self.pool, DEFAULT_THEME_KEY)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(DEFAULT_THEME_KEY.to_string()))?;
        Ok(TenantTheme::default_draft(
            tenant_id,
            theme.id,
            theme.key,
            version.semver,
            self.clock.now(),
        ))
    }
}

/// `PUT /api/v1/tenant/theme/draft`.
pub struct UpdateTenantThemeDraft {
    themes: Arc<dyn ThemeRepository>,
    tenants: Arc<dyn TenantThemeRepository>,
    gate: Arc<dyn ThemeFeatureGate>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    pool: PgPool,
    custom_css_max_bytes: usize,
}

impl UpdateTenantThemeDraft {
    pub fn new(
        themes: Arc<dyn ThemeRepository>,
        tenants: Arc<dyn TenantThemeRepository>,
        gate: Arc<dyn ThemeFeatureGate>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        pool: PgPool,
        custom_css_max_bytes: usize,
    ) -> Self {
        Self {
            themes,
            tenants,
            gate,
            events,
            clock,
            pool,
            custom_css_max_bytes,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: TenantId,
        updated_by: Option<UserId>,
        locale: &str,
        command: &UpdateTenantThemeDraftCommand,
    ) -> Result<TenantThemeView, ThemingError> {
        let mut theme = match self
            .tenants
            .get(scope, tenant_id, ThemeState::Draft)
            .await?
        {
            Some(draft) => draft,
            None => self.default_draft(tenant_id).await?,
        };

        let validator = DraftValidator::new(
            &self.themes,
            &self.gate,
            &self.pool,
            self.custom_css_max_bytes,
        );
        let (theme_id, version) = validator.target_version(tenant_id, &theme, command).await?;

        let token_overrides = command
            .token_overrides
            .clone()
            .unwrap_or_else(|| theme.token_overrides.clone());
        let layout_overrides = command
            .layout_overrides
            .clone()
            .unwrap_or_else(|| theme.layout_overrides.clone());
        let custom_css = match &command.custom_css {
            Some(Some(css)) => Some(css.clone()),
            Some(None) => None,
            None => theme.custom_css.clone(),
        };

        let validated = validator
            .validate_adjustments(
                tenant_id,
                &version,
                &token_overrides,
                &layout_overrides,
                custom_css.as_deref(),
            )
            .await?;

        theme.theme_id = theme_id;
        theme.theme_key = version.manifest.id.clone();
        theme.semver = version.semver.clone();
        theme.token_overrides = token_overrides;
        theme.layout_overrides = layout_overrides;
        theme.custom_css = validated.custom_css;
        theme.updated_by = updated_by;
        theme.updated_at = self.clock.now();

        self.tenants.upsert(scope, &theme).await?;
        self.events
            .publish_scoped(
                scope,
                &ThemingEvent::TenantThemeChanged {
                    tenant_id,
                    locale: locale.to_string(),
                },
            )
            .await?;

        Ok(to_view(&theme))
    }

    async fn default_draft(&self, tenant_id: TenantId) -> Result<TenantTheme, ThemingError> {
        let theme = self
            .themes
            .get_by_key(&self.pool, DEFAULT_THEME_KEY)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(DEFAULT_THEME_KEY.to_string()))?;
        let version = self
            .themes
            .get_latest_version(&self.pool, DEFAULT_THEME_KEY)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(DEFAULT_THEME_KEY.to_string()))?;
        Ok(TenantTheme::default_draft(
            tenant_id,
            theme.id,
            theme.key,
            version.semver,
            self.clock.now(),
        ))
    }
}

pub fn to_view(theme: &TenantTheme) -> TenantThemeView {
    TenantThemeView {
        state: theme.state,
        theme_key: theme.theme_key.clone(),
        version: theme.semver.to_string(),
        token_overrides: theme.token_overrides.clone(),
        layout_overrides: theme.layout_overrides.clone(),
        assets: theme.assets.clone(),
        custom_css: theme.custom_css.clone(),
        revision: theme.revision,
        updated_at: theme.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn custom_css_distinguishes_missing_null_and_value() {
        let missing: UpdateTenantThemeDraftCommand = serde_json::from_value(json!({})).unwrap();
        assert_eq!(missing.custom_css, None);

        let cleared: UpdateTenantThemeDraftCommand =
            serde_json::from_value(json!({ "custom_css": null })).unwrap();
        assert_eq!(cleared.custom_css, Some(None));

        let replaced: UpdateTenantThemeDraftCommand =
            serde_json::from_value(json!({ "custom_css": ".brand {}" })).unwrap();
        assert_eq!(replaced.custom_css, Some(Some(".brand {}".to_string())));
    }
}
