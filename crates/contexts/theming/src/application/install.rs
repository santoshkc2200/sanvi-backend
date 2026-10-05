use std::sync::Arc;

use sanvi_kernel::RawIdGenerator;
use sanvi_persistence::PgPool;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::views::{ThemeAdminView, ThemeVersionAdminView};
use crate::domain::contrast::check_theme;
use crate::domain::error::ThemingError;
use crate::domain::manifest::Manifest;
use crate::domain::theme::{
    Theme, ThemeId, ThemeStatus, ThemeVersion, ThemeVersionId, ThemeVisibility, TokenModes,
};
use crate::domain::{ArtifactStore, Clock, EventPublisher, ThemeRepository, ThemingEvent};

/// The command a theme artifact resolves to after the publish pipeline
/// validated it (manifest schema, token schema, contrast gate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InstallThemeVersionCommand {
    /// The validated manifest (`theme.json`).
    pub manifest: serde_json::Value,
    /// The merged token document `{light: …, dark: …}`.
    pub tokens: serde_json::Value,
    /// Content-addressed artifact location.
    pub artifact_uri: String,
    /// sha256 digest the artifact must match.
    pub artifact_digest: String,
    /// Who published (first-party "sanvi" or a future third party).
    pub publisher: String,
    /// `public` or `premium`.
    pub visibility: String,
    /// For premium themes: the entitlement key.
    pub required_feature: Option<String>,
    /// Explicit override for the contrast gate (recorded, never silent).
    pub allow_contrast_warnings: bool,
}

/// `POST /api/v1/platform/themes` (+ versions): install a theme version.
///
/// The same validation the CLI's `theme publish` pipeline runs: manifest
/// schema, `themeApi` range, DTCG token schema, the WCAG AA contrast gate.
pub struct InstallThemeVersion {
    themes: Arc<dyn ThemeRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    pool: PgPool,
    artifact_store: Arc<dyn ArtifactStore>,
    ids: Arc<dyn RawIdGenerator>,
}

impl InstallThemeVersion {
    pub fn new(
        themes: Arc<dyn ThemeRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        pool: PgPool,
        artifact_store: Arc<dyn ArtifactStore>,
        ids: Arc<dyn RawIdGenerator>,
    ) -> Self {
        Self {
            themes,
            events,
            clock,
            pool,
            artifact_store,
            ids,
        }
    }

    #[tracing::instrument(name = "theming.install_theme_version", skip_all)]
    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        command: &InstallThemeVersionCommand,
    ) -> Result<ThemeVersionAdminView, ThemingError> {
        // 1. The manifest must be schema-valid and themeApi-compatible.
        let manifest = Manifest::parse(&command.manifest)?;

        validate_artifact_reference(&command.artifact_uri, &command.artifact_digest)?;
        self.artifact_store
            .verify(&command.artifact_uri, &command.artifact_digest)
            .await?;

        // 2. Tokens must validate as DTCG documents.
        let tokens = TokenModes::parse(&command.tokens)?;

        // 3. Contrast gate: semantic pairs must meet WCAG AA. An explicit
        // override is recorded (never silent).
        if !command.allow_contrast_warnings {
            check_theme(&tokens.light, tokens.dark.as_ref())?;
        } else {
            tracing::warn!(
                theme = %manifest.id,
                version = %manifest.version,
                "theme published with contrast warnings explicitly allowed"
            );
        }

        let visibility = ThemeVisibility::parse(&command.visibility)?;

        // 4. New theme? Create the registry row; otherwise reuse it.
        let (theme_id, theme_to_insert) =
            match self.themes.get_by_key(&self.pool, &manifest.id).await? {
                Some(existing) => (existing.id, None),
                None => {
                    let id = ThemeId::from_snowflake(self.ids.next_id()?);
                    (
                        id,
                        Some(Theme {
                            id,
                            key: manifest.id.clone(),
                            publisher: command.publisher.clone(),
                            visibility,
                            required_feature: command.required_feature.clone(),
                            status: ThemeStatus::Active,
                            created_at: self.clock.now(),
                        }),
                    )
                }
            };

        // 5. Immutable version row (the trigger enforces this too).
        let exists = self
            .themes
            .list_versions(&self.pool, theme_id)
            .await?
            .iter()
            .any(|version| version.semver.to_string() == manifest.version);
        if exists {
            return Err(ThemingError::ThemeVersionExists {
                theme: manifest.id.clone(),
                version: manifest.version.clone(),
            });
        }

        let semver = semver::Version::parse(&manifest.version).map_err(|err| {
            ThemingError::InvalidManifest(format!("version {:?}: {err}", manifest.version))
        })?;
        let theme_api_range = semver::VersionReq::parse(&manifest.theme_api).map_err(|err| {
            ThemingError::InvalidManifest(format!("themeApi {:?}: {err}", manifest.theme_api))
        })?;

        let version_id = ThemeVersionId::from_snowflake(self.ids.next_id()?);
        let version = ThemeVersion {
            id: version_id,
            theme_id,
            semver,
            theme_api_range,
            manifest,
            tokens,
            artifact_uri: command.artifact_uri.clone(),
            artifact_digest: command.artifact_digest.clone(),
            published_at: self.clock.now(),
            yanked_at: None,
        };

        // Writes run in the audited platform scope, atomically with the
        // published event.
        if let Some(theme) = &theme_to_insert {
            self.themes.insert(scope, theme).await?;
        }
        self.themes.insert_version(scope, &version).await?;
        self.events
            .publish(
                scope,
                &ThemingEvent::ThemePublished {
                    theme_key: version.manifest.id.clone(),
                    version: version.semver.to_string(),
                },
            )
            .await?;

        Ok((&version).into())
    }
}

fn validate_artifact_reference(uri: &str, digest: &str) -> Result<(), ThemingError> {
    let path = std::path::Path::new(uri);
    let safe_uri = !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)));
    if !safe_uri {
        return Err(ThemingError::InvalidManifest(
            "artifact_uri must be a relative artifact key".to_string(),
        ));
    }
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ThemingError::InvalidManifest(
            "artifact_digest must be a 64-character sha256 hex digest".to_string(),
        ));
    }
    Ok(())
}

/// `POST /api/v1/platform/themes/{key}/versions/{version}/yank` — the kill
/// switch. Tenants pinned to the version keep rendering; new selections
/// fail.
pub struct YankThemeVersion {
    themes: Arc<dyn ThemeRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    pool: PgPool,
}

impl YankThemeVersion {
    pub fn new(
        themes: Arc<dyn ThemeRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        pool: PgPool,
    ) -> Self {
        Self {
            themes,
            events,
            clock,
            pool,
        }
    }

    #[tracing::instrument(name = "theming.yank_theme_version", skip_all)]
    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        theme_key: &str,
        version: &str,
    ) -> Result<(), ThemingError> {
        let theme = self
            .themes
            .get_by_key(&self.pool, theme_key)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(theme_key.to_string()))?;
        self.themes
            .yank_version(scope, theme.id, version, self.clock.now())
            .await?;
        self.events
            .publish(
                scope,
                &ThemingEvent::ThemeYanked {
                    theme_key: theme_key.to_string(),
                    version: version.to_string(),
                },
            )
            .await?;
        Ok(())
    }
}

/// `GET /api/v1/platform/themes` — the full registry with versions.
pub struct ListThemesAdmin {
    themes: Arc<dyn ThemeRepository>,
    pool: PgPool,
}

impl ListThemesAdmin {
    pub fn new(themes: Arc<dyn ThemeRepository>, pool: PgPool) -> Self {
        Self { themes, pool }
    }

    #[tracing::instrument(name = "theming.list_themes_admin", skip_all)]
    pub async fn execute(&self) -> Result<Vec<ThemeAdminView>, ThemingError> {
        let registry = self.themes.list(&self.pool).await?;
        let mut views = Vec::with_capacity(registry.len());
        for theme in registry {
            let versions = self.themes.list_versions(&self.pool, theme.id).await?;
            views.push(ThemeAdminView::from_theme(&theme, &versions));
        }
        views.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(views)
    }
}

/// `GET /api/v1/platform/themes/{key}/versions` — one theme's version
/// history.
pub struct ListThemeVersionsAdmin {
    themes: Arc<dyn ThemeRepository>,
    pool: PgPool,
}

impl ListThemeVersionsAdmin {
    pub fn new(themes: Arc<dyn ThemeRepository>, pool: PgPool) -> Self {
        Self { themes, pool }
    }

    #[tracing::instrument(name = "theming.list_theme_versions_admin", skip_all)]
    pub async fn execute(
        &self,
        theme_key: &str,
        limit: Option<u32>,
        after_id: Option<i64>,
    ) -> Result<Vec<ThemeVersionAdminView>, ThemingError> {
        let limit = sanvi_kernel::pagination::clamp_page_size(limit, 200, 200);
        let theme = self
            .themes
            .get_by_key(&self.pool, theme_key)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(theme_key.to_string()))?;
        let versions = self
            .themes
            .list_versions_page(&self.pool, theme.id, limit, after_id)
            .await?;
        Ok(versions.iter().map(ThemeVersionAdminView::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_artifact_reference;

    #[test]
    fn artifact_reference_rejects_traversal_and_bad_digests() {
        assert!(validate_artifact_reference("theme.zip", &"a".repeat(64)).is_ok());
        assert!(validate_artifact_reference("../theme.zip", &"a".repeat(64)).is_err());
        assert!(validate_artifact_reference("/theme.zip", &"a".repeat(64)).is_err());
        assert!(validate_artifact_reference("theme.zip", "not-a-digest").is_err());
    }
}
