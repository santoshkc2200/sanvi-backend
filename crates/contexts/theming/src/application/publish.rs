//! The publish workflow: draft → live with an immutable snapshot, and the
//! one-click rollback that restores the previous snapshot exactly.

use std::sync::Arc;

use sanvi_persistence::PgPool;
use uuid::Uuid;

use crate::application::draft::{DraftValidator, to_view};
use crate::application::views::TenantThemeView;
use crate::domain::contrast::check_theme;
use crate::domain::error::ThemingError;
use crate::domain::tenant_theme::{TenantTheme, ThemeState};
use crate::domain::{
    Clock, EventPublisher, TenantThemeRepository, ThemeFeatureGate, ThemeRepository, ThemingEvent,
};

/// `POST /api/v1/tenant/theme/publish` — the draft becomes the live theme.
///
/// Publishing is atomic: the draft is copied to the live row, the new state
/// is snapshotted as the next revision, and the event that invalidates
/// every resolved-theme cache is appended in the same transaction.
pub struct PublishTenantTheme {
    themes: Arc<dyn ThemeRepository>,
    tenants: Arc<dyn TenantThemeRepository>,
    gate: Arc<dyn ThemeFeatureGate>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    pool: PgPool,
    custom_css_max_bytes: usize,
    cache: Arc<crate::adapters::cache::ResolvedThemeCache>,
}

impl PublishTenantTheme {
    pub fn new(
        themes: Arc<dyn ThemeRepository>,
        tenants: Arc<dyn TenantThemeRepository>,
        gate: Arc<dyn ThemeFeatureGate>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        pool: PgPool,
        custom_css_max_bytes: usize,
        cache: Arc<crate::adapters::cache::ResolvedThemeCache>,
    ) -> Self {
        Self {
            themes,
            tenants,
            gate,
            events,
            clock,
            pool,
            custom_css_max_bytes,
            cache,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: Uuid,
        published_by: Option<Uuid>,
        locale: &str,
    ) -> Result<TenantThemeView, ThemingError> {
        let draft = self
            .tenants
            .get(scope, tenant_id, ThemeState::Draft)
            .await?
            .ok_or_else(|| {
                ThemingError::InvalidState("there is no draft to publish".to_string())
            })?;

        // Re-validate everything, including the contrast gate: a draft can
        // only become live when the resolved palette passes WCAG AA.
        self.validate_draft(tenant_id, &draft).await?;

        let revision = self
            .tenants
            .publish(scope, tenant_id, published_by, self.clock.now())
            .await?;
        self.events
            .publish_scoped(
                scope,
                &ThemingEvent::TenantThemePublished {
                    tenant_id,
                    theme_key: draft.theme_key.clone(),
                    version: draft.semver.to_string(),
                    revision,
                    locale: locale.to_string(),
                },
            )
            .await?;

        let live = self
            .tenants
            .get(scope, tenant_id, ThemeState::Live)
            .await?
            .ok_or_else(|| {
                ThemingError::InvalidState(
                    "publish succeeded but the live row is missing".to_string(),
                )
            })?;
        // In-process cache: the commit that made the change is about to
        // complete, so the instance serving the request must forget the old
        // resolved theme before it answers the next one.
        self.cache.invalidate_local(tenant_id);
        Ok(to_view(&live))
    }

    async fn validate_draft(
        &self,
        tenant_id: Uuid,
        draft: &TenantTheme,
    ) -> Result<(), ThemingError> {
        let version = self
            .themes
            .get_version(&self.pool, &draft.theme_key, &draft.semver.to_string())
            .await?
            .ok_or_else(|| ThemingError::UnknownThemeVersion {
                theme: draft.theme_key.clone(),
                version: draft.semver.to_string(),
            })?;
        let validator = DraftValidator::new(
            &self.themes,
            &self.gate,
            &self.pool,
            self.custom_css_max_bytes,
        );
        let validated = validator
            .validate_adjustments(
                tenant_id,
                &version,
                &draft.token_overrides,
                &draft.layout_overrides,
                draft.custom_css.as_deref(),
            )
            .await?;
        check_theme(&validated.tokens_light, validated.tokens_dark.as_ref())?;
        Ok(())
    }
}

/// `POST /api/v1/tenant/theme/rollback` — restore the previous snapshot.
pub struct RollbackTenantTheme {
    tenants: Arc<dyn TenantThemeRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    cache: Arc<crate::adapters::cache::ResolvedThemeCache>,
}

impl RollbackTenantTheme {
    pub fn new(
        tenants: Arc<dyn TenantThemeRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        cache: Arc<crate::adapters::cache::ResolvedThemeCache>,
    ) -> Self {
        Self {
            tenants,
            events,
            clock,
            cache,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: Uuid,
        published_by: Option<Uuid>,
        locale: &str,
    ) -> Result<TenantThemeView, ThemingError> {
        let revision = self
            .tenants
            .rollback(scope, tenant_id, published_by, self.clock.now())
            .await?;
        self.events
            .publish_scoped(
                scope,
                &ThemingEvent::TenantThemeRolledBack {
                    tenant_id,
                    revision,
                    locale: locale.to_string(),
                },
            )
            .await?;
        let live = self
            .tenants
            .get(scope, tenant_id, ThemeState::Live)
            .await?
            .ok_or_else(|| {
                ThemingError::InvalidState(
                    "rollback succeeded but the live row is missing".to_string(),
                )
            })?;
        self.cache.invalidate_local(tenant_id);
        Ok(to_view(&live))
    }
}
