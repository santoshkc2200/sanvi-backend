//! Application state visible to this context's handlers.

use std::sync::Arc;

use sanvi_persistence::PgPool;

use crate::application::{
    InstallThemeVersion, ListAvailableThemes, ListThemeVersionsAdmin, ListThemesAdmin,
    PreviewTenantTheme, PublishTenantTheme, RemoveBrandAsset, ResolvePublicTheme,
    ResolveThemeService, RollbackTenantTheme, UpdateTenantThemeDraft, UploadBrandAsset,
    draft::GetTenantThemeDraft,
};
use crate::config::ThemingConfig;

#[derive(Clone)]
pub struct ThemingState {
    pub pool: PgPool,
    pub tx_manager: sanvi_persistence::TxManager,
    pub config: ThemingConfig,
    /// The shared resolution service (public endpoint, live resolution).
    pub service: Arc<ResolveThemeService>,

    pub get_public_theme: Arc<ResolvePublicTheme>,
    pub preview_theme: Arc<PreviewTenantTheme>,
    pub get_draft: Arc<GetTenantThemeDraft>,
    pub update_draft: Arc<UpdateTenantThemeDraft>,
    pub publish: Arc<PublishTenantTheme>,
    pub rollback: Arc<RollbackTenantTheme>,
    pub available: Arc<ListAvailableThemes>,
    pub upload_asset: Arc<UploadBrandAsset>,
    pub remove_asset: Arc<RemoveBrandAsset>,
    pub install_version: Arc<InstallThemeVersion>,
    pub yank_version: Arc<crate::application::install::YankThemeVersion>,
    pub list_themes: Arc<ListThemesAdmin>,
    pub list_versions: Arc<ListThemeVersionsAdmin>,
}
