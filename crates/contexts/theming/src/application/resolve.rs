//! Theme resolution: `tenant + theme version + overrides + locale` → one
//! `ResolvedTheme` document with a precompiled CSS custom-property block.
//!
//! Live resolutions are cached (Redis with a process-local fallback, TTL
//! from config) and invalidated by publish/rollback events; the ETag is a
//! hash of everything that changes the rendered output. Draft previews
//! never touch the cache.

use std::sync::Arc;

use sanvi_cache::SingleFlight;
use sanvi_kernel::{RawIdGenerator, TenantId};
use sanvi_persistence::PgPool;

use crate::adapters::cache::ResolvedThemeCache;
use crate::adapters::metrics::ThemingMetrics;
use crate::domain::css::emit_css_vars;
use crate::domain::error::ThemingError;
use crate::domain::resolved::{
    BrandAssetUrls, CUSTOM_CSS_SCOPE_CLASS, ResolvedLayout, ResolvedTheme, ThemeAssetUrls,
    resolution_etag,
};
use crate::domain::tenant_theme::{TenantTheme, ThemeState};
use crate::domain::theme::{DEFAULT_THEME_KEY, ThemeVersion};
use crate::domain::tokens::{TokenNode, apply_overrides, merge_tokens};
use crate::domain::{Clock, TenantContextResolver, TenantThemeRepository, ThemeRepository};

/// The resolution service: the single place a `ResolvedTheme` is built.
pub struct ResolveThemeService {
    themes: Arc<dyn ThemeRepository>,
    tenants: Arc<dyn TenantThemeRepository>,
    resolver: Arc<dyn TenantContextResolver>,
    cache: Arc<ResolvedThemeCache>,
    single_flight: Arc<SingleFlight>,
    metrics: Arc<ThemingMetrics>,
    clock: Arc<dyn Clock>,
    tx: sanvi_persistence::TxManager,
    asset_base_url: String,
    ttl_secs: u64,
}

impl ResolveThemeService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        themes: Arc<dyn ThemeRepository>,
        tenants: Arc<dyn TenantThemeRepository>,
        resolver: Arc<dyn TenantContextResolver>,
        cache: Arc<ResolvedThemeCache>,
        metrics: Arc<ThemingMetrics>,
        clock: Arc<dyn Clock>,
        pool: PgPool,
        ids: Arc<dyn RawIdGenerator>,
        asset_base_url: String,
        ttl_secs: u64,
    ) -> Self {
        Self {
            themes,
            tenants,
            resolver,
            cache,
            single_flight: Arc::new(SingleFlight::new()),
            metrics,
            clock,
            tx: sanvi_persistence::TxManager::new(pool, ids),
            asset_base_url,
            ttl_secs,
        }
    }

    pub fn cache(&self) -> Arc<ResolvedThemeCache> {
        self.cache.clone()
    }

    /// The public endpoint: host-resolved tenant (or the platform default
    /// when the host has no tenant — the marketing site).
    pub async fn resolve_public(
        &self,
        host: &str,
        locale: &str,
    ) -> Result<ResolvedTheme, ThemingError> {
        let _timer = self.metrics.resolution_duration.start_timer();
        let tenant = self.resolver.resolve(host).await?;
        match tenant {
            Some(tenant) => self.resolve_live(tenant.tenant_id, locale).await,
            None => self.platform_default(locale).await,
        }
    }

    /// A tenant's live theme, cache-first with single-flight stampede
    /// protection (TASK-021 11.2g): one resolution per tenant+locale per
    /// expiry; concurrent misses wait and re-read the cache instead of each
    /// rebuilding the resolved document against the database.
    pub async fn resolve_live(
        &self,
        tenant_id: TenantId,
        locale: &str,
    ) -> Result<ResolvedTheme, ThemingError> {
        let _timer = self.metrics.resolution_duration.start_timer();
        self.single_flight
            .get_or_load(
                &format!("theming:resolved:{tenant_id}:{locale}"),
                || {
                    Box::pin(async {
                        match self.cache.get(tenant_id, locale).await {
                            Some(hit) => {
                                self.metrics.cache_hits.inc();
                                Some(hit)
                            }
                            None => {
                                self.metrics.cache_misses.inc();
                                None
                            }
                        }
                    })
                },
                |resolved| {
                    Box::pin(async move {
                        self.cache
                            .set(tenant_id, locale, &resolved, self.ttl_secs)
                            .await;
                    })
                },
                || Box::pin(async { self.resolve_live_uncached(tenant_id, locale).await }),
            )
            .await
    }

    /// The uncached resolution path (one process-local flight at a time).
    async fn resolve_live_uncached(
        &self,
        tenant_id: TenantId,
        locale: &str,
    ) -> Result<ResolvedTheme, ThemingError> {
        let mut scope =
            self.tx.begin_for(tenant_id.as_i64()).await.map_err(|err| {
                ThemingError::Storage(format!("failed to open tenant scope: {err}"))
            })?;
        let row = self
            .tenants
            .get(&mut scope, tenant_id, ThemeState::Live)
            .await?;
        let resolved = match row {
            Some(live) => self.build(&live, locale).await?,
            None => self.default_theme(tenant_id, locale).await?,
        };
        scope
            .commit()
            .await
            .map_err(|err| ThemingError::Storage(err.to_string()))?;
        Ok(resolved)
    }

    /// The draft preview: same document shape, revision 0, never cached.
    pub async fn preview_draft(
        &self,
        tenant_id: TenantId,
        locale: &str,
    ) -> Result<ResolvedTheme, ThemingError> {
        let _timer = self.metrics.resolution_duration.start_timer();
        let mut scope =
            self.tx.begin_for(tenant_id.as_i64()).await.map_err(|err| {
                ThemingError::Storage(format!("failed to open tenant scope: {err}"))
            })?;
        let draft = self
            .tenants
            .get(&mut scope, tenant_id, ThemeState::Draft)
            .await?;
        let mut resolved = match draft {
            Some(draft) => self.build(&draft, locale).await?,
            None => self.default_theme(tenant_id, locale).await?,
        };
        scope
            .commit()
            .await
            .map_err(|err| ThemingError::Storage(err.to_string()))?;

        // Previews are never cacheable and never carry a live revision.
        resolved.revision = 0;
        resolved.etag = resolution_etag(&resolved.theme_key, &resolved.theme_version, 0, locale);
        Ok(resolved)
    }

    /// The platform default: the base theme for hosts without a tenant.
    async fn platform_default(&self, locale: &str) -> Result<ResolvedTheme, ThemingError> {
        self.default_theme(TenantId::try_from(1_i64).unwrap(), locale)
            .await
    }

    /// A tenant with no live row: the base theme at its latest version.
    /// Built inline (never through [`Self::build`]) so the fail-closed
    /// fallback path in `build` cannot recurse.
    async fn default_theme(
        &self,
        tenant_id: TenantId,
        locale: &str,
    ) -> Result<ResolvedTheme, ThemingError> {
        let theme = self
            .themes
            .get_by_key(self.tx.pool(), DEFAULT_THEME_KEY)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(DEFAULT_THEME_KEY.to_string()))?;
        let version = self
            .themes
            .get_latest_version(self.tx.pool(), DEFAULT_THEME_KEY)
            .await?
            .ok_or_else(|| ThemingError::UnknownTheme(DEFAULT_THEME_KEY.to_string()))?;
        let row = TenantTheme::default_draft(
            tenant_id,
            theme.id,
            DEFAULT_THEME_KEY.to_string(),
            version.semver.clone(),
            self.clock.now(),
        );
        self.build_with(&row, &version, None, None, locale)
    }

    /// Build the resolved document for a tenant theme row.
    ///
    /// Failure semantics: when the pinned version is missing, resolution
    /// fails closed to the base theme so the storefront never dies with its
    /// theme. (Yanked versions keep rendering — a pinned tenant is never
    /// torn down by a withdrawal.)
    async fn build(&self, row: &TenantTheme, locale: &str) -> Result<ResolvedTheme, ThemingError> {
        let version = match self
            .themes
            .get_version(self.tx.pool(), &row.theme_key, &row.semver.to_string())
            .await?
        {
            Some(version) => version,
            None => {
                tracing::error!(
                    tenant_id = %row.tenant_id,
                    theme = %row.theme_key,
                    version = %row.semver,
                    "pinned theme version missing; failing closed to the base theme"
                );
                return self.default_theme(row.tenant_id, locale).await;
            }
        };

        // The base theme's tokens are the platform fallback layer: merge
        // them under the theme's own tokens so a theme can never leave a
        // semantic token undefined.
        let (base_light, base_dark) = if row.theme_key == DEFAULT_THEME_KEY {
            (None, None)
        } else {
            let base = self
                .themes
                .get_latest_version(self.tx.pool(), DEFAULT_THEME_KEY)
                .await?;
            (
                base.as_ref().map(|base| base.tokens.light.clone()),
                base.and_then(|base| base.tokens.dark.clone()),
            )
        };

        self.build_with(
            row,
            &version,
            base_light.as_ref(),
            base_dark.as_ref(),
            locale,
        )
    }

    fn build_with(
        &self,
        row: &TenantTheme,
        version: &ThemeVersion,
        base_light: Option<&TokenNode>,
        base_dark: Option<&TokenNode>,
        locale: &str,
    ) -> Result<ResolvedTheme, ThemingError> {
        let merged_light = merge_tokens(
            base_light.unwrap_or(&version.tokens.light),
            &version.tokens.light,
        );
        let (tokens_light, tokens_dark) = match &version.tokens.dark {
            Some(theme_dark) => {
                let merged_dark = merge_tokens(base_dark.unwrap_or(theme_dark), theme_dark);
                (
                    apply_overrides(&merged_light, &row.token_overrides, &version.manifest)?,
                    Some(apply_overrides(
                        &merged_dark,
                        &row.token_overrides,
                        &version.manifest,
                    )?),
                )
            }
            None => (
                apply_overrides(&merged_light, &row.token_overrides, &version.manifest)?,
                None,
            ),
        };

        let css_vars = emit_css_vars(&tokens_light, tokens_dark.as_ref());
        let (tokens_value, dark_tokens_value) = (
            serde_json::to_value(&tokens_light).expect("token trees serialize"),
            tokens_dark
                .as_ref()
                .map(|dark| serde_json::to_value(dark).expect("token trees serialize")),
        );

        let layouts = version
            .manifest
            .layouts
            .iter()
            .map(|(key, spec)| {
                (
                    key.clone(),
                    ResolvedLayout::from_slots(&spec.slots, row.layout_overrides.get(key)),
                )
            })
            .collect();

        let custom_css = row
            .custom_css
            .as_ref()
            .map(|css| crate::domain::sanitizer::scope_css(css, CUSTOM_CSS_SCOPE_CLASS));

        let etag = resolution_etag(
            &row.theme_key,
            &row.semver.to_string(),
            row.revision,
            locale,
        );

        Ok(ResolvedTheme {
            theme_key: row.theme_key.clone(),
            theme_version: row.semver.to_string(),
            theme_api: crate::domain::manifest::THEME_API_VERSION.to_string(),
            capabilities: version.manifest.capabilities.clone(),
            tokens: tokens_value,
            dark_tokens: dark_tokens_value,
            css_vars,
            layouts,
            fonts: version.manifest.fonts.clone(),
            theme_assets: ThemeAssetUrls::resolve(
                &row.theme_key,
                &row.semver,
                version.manifest.assets.preview.as_ref(),
                &version.manifest.assets.screenshots,
                &self.asset_base_url,
            ),
            brand_assets: BrandAssetUrls::resolve(&row.assets, &self.asset_base_url, row.tenant_id),
            custom_css,
            revision: row.revision,
            locale: locale.to_string(),
            etag,
        })
    }
}

/// The HTTP-facing wrapper: `GET /api/v1/public/theme`.
pub struct ResolvePublicTheme {
    service: Arc<ResolveThemeService>,
}

impl ResolvePublicTheme {
    pub fn new(service: Arc<ResolveThemeService>) -> Self {
        Self { service }
    }

    #[tracing::instrument(name = "theming.resolve_public_theme", skip_all)]
    pub async fn execute(&self, host: &str, locale: &str) -> Result<ResolvedTheme, ThemingError> {
        self.service.resolve_public(host, locale).await
    }
}

/// The HTTP-facing wrapper: `GET /api/v1/tenant/theme/preview`.
pub struct PreviewTenantTheme {
    service: Arc<ResolveThemeService>,
}

impl PreviewTenantTheme {
    pub fn new(service: Arc<ResolveThemeService>) -> Self {
        Self { service }
    }

    #[tracing::instrument(name = "theming.preview_tenant_theme", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        locale: &str,
    ) -> Result<ResolvedTheme, ThemingError> {
        self.service.preview_draft(tenant_id, locale).await
    }
}
