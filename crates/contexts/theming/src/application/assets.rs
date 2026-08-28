use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sanvi_kernel::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::draft::to_view;
use crate::application::views::TenantThemeView;
use crate::domain::error::ThemingError;
use crate::domain::tenant_theme::{BrandAsset, BrandAssetKind, ThemeState};
use crate::domain::{
    BrandAssetStore, Clock, EventPublisher, TenantThemeRepository, ThemeRepository, ThemingEvent,
    validate_asset,
};

/// The upload command: base64 payload with a declared media type (the
/// declared type is advisory — the bytes are sniffed and must match).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UploadBrandAssetCommand {
    /// `logo` | `favicon` | `og_image`.
    pub kind: String,
    /// The declared media type (must agree with the sniffed bytes).
    #[serde(default)]
    pub content_type: Option<String>,
    /// Base64-encoded image bytes.
    pub data_base64: String,
}

/// `POST /api/v1/tenant/theme/assets`.
pub struct UploadBrandAsset {
    tenants: Arc<dyn TenantThemeRepository>,
    themes: Arc<dyn ThemeRepository>,
    store: Arc<dyn BrandAssetStore>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    pool: sanvi_persistence::PgPool,
    max_bytes: usize,
}

impl UploadBrandAsset {
    pub fn new(
        tenants: Arc<dyn TenantThemeRepository>,
        themes: Arc<dyn ThemeRepository>,
        store: Arc<dyn BrandAssetStore>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        pool: sanvi_persistence::PgPool,
        max_bytes: usize,
    ) -> Self {
        Self {
            tenants,
            themes,
            store,
            events,
            clock,
            pool,
            max_bytes,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: TenantId,
        updated_by: Option<UserId>,
        locale: &str,
        command: &UploadBrandAssetCommand,
    ) -> Result<TenantThemeView, ThemingError> {
        let kind = BrandAssetKind::parse(&command.kind)?;

        let decoded = STANDARD.decode(&command.data_base64).map_err(|err| {
            ThemingError::InvalidBrandAsset(format!("data_base64 is invalid: {err}"))
        })?;
        if decoded.len() > self.max_bytes {
            return Err(ThemingError::InvalidBrandAsset(format!(
                "asset is {} bytes, over the {} byte cap",
                decoded.len(),
                self.max_bytes
            )));
        }

        // Sniff + clean (EXIF strip, dimension caps, format allow-list).
        let validated = validate_asset(kind, &decoded)?;

        // The declared type, when present, must agree with the sniff.
        if let Some(declared) = &command.content_type
            && !declared.is_empty()
            && declared != validated.mime
        {
            return Err(ThemingError::InvalidBrandAsset(format!(
                "content_type {declared:?} does not match the file content ({})",
                validated.mime
            )));
        }

        let digest = validated.digest_hex();
        self.store
            .put(tenant_id, &digest, validated.ext, &validated.bytes)
            .await?;

        let mut draft = match self
            .tenants
            .get(scope, tenant_id, ThemeState::Draft)
            .await?
        {
            Some(draft) => draft,
            None => self.default_draft(tenant_id).await?,
        };
        let asset = BrandAsset {
            kind,
            digest,
            mime: validated.mime.to_string(),
            ext: validated.ext.to_string(),
            uploaded_at: self.clock.now(),
        };
        draft.assets.set(asset);
        draft.updated_by = updated_by;
        draft.updated_at = self.clock.now();
        self.tenants.upsert(scope, &draft).await?;
        self.events
            .publish_scoped(
                scope,
                &ThemingEvent::BrandAssetUpdated {
                    tenant_id,
                    asset_kind: kind.as_str().to_string(),
                    locale: locale.to_string(),
                },
            )
            .await?;
        Ok(to_view(&draft))
    }

    async fn default_draft(
        &self,
        tenant_id: TenantId,
    ) -> Result<crate::domain::tenant_theme::TenantTheme, ThemingError> {
        let theme = self
            .themes
            .get_by_key(&self.pool, crate::domain::DEFAULT_THEME_KEY)
            .await?
            .ok_or_else(|| {
                ThemingError::UnknownTheme(crate::domain::DEFAULT_THEME_KEY.to_string())
            })?;
        let version = self
            .themes
            .get_latest_version(&self.pool, crate::domain::DEFAULT_THEME_KEY)
            .await?
            .ok_or_else(|| {
                ThemingError::UnknownTheme(crate::domain::DEFAULT_THEME_KEY.to_string())
            })?;
        Ok(crate::domain::tenant_theme::TenantTheme::default_draft(
            tenant_id,
            theme.id,
            theme.key,
            version.semver,
            self.clock.now(),
        ))
    }
}

/// `DELETE /api/v1/tenant/theme/assets/{kind}`.
pub struct RemoveBrandAsset {
    tenants: Arc<dyn TenantThemeRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl RemoveBrandAsset {
    pub fn new(
        tenants: Arc<dyn TenantThemeRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tenants,
            events,
            clock,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: TenantId,
        updated_by: Option<UserId>,
        locale: &str,
        kind: BrandAssetKind,
    ) -> Result<TenantThemeView, ThemingError> {
        let Some(mut draft) = self
            .tenants
            .get(scope, tenant_id, ThemeState::Draft)
            .await?
        else {
            return Err(ThemingError::InvalidBrandAsset(
                "there is no draft to remove an asset from".to_string(),
            ));
        };
        match kind {
            BrandAssetKind::Logo => draft.assets.logo.take(),
            BrandAssetKind::Favicon => draft.assets.favicon.take(),
            BrandAssetKind::OgImage => draft.assets.og_image.take(),
        };
        // Brand objects are content-addressed and may still be referenced by
        // the live row or an immutable revision. Physical deletion belongs
        // to a reference-aware garbage collector, not a draft mutation.
        draft.updated_by = updated_by;
        draft.updated_at = self.clock.now();
        self.tenants.upsert(scope, &draft).await?;
        self.events
            .publish_scoped(
                scope,
                &ThemingEvent::BrandAssetUpdated {
                    tenant_id,
                    asset_kind: kind.as_str().to_string(),
                    locale: locale.to_string(),
                },
            )
            .await?;
        Ok(to_view(&draft))
    }
}
