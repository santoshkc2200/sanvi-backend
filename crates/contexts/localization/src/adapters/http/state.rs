//! Application state visible to this context's handlers.

use std::sync::Arc;

use sanvi_http::{LocaleResolver, ProblemTranslator};
use sanvi_persistence::PgPool;

use crate::adapters::catalog::LocalizationService;
use crate::application::{
    CatalogService, GetLocalizationSettings, GetPlatformTranslations, GetTenantOverrides,
    UpdateLocalizationSettings, UpdatePlatformTranslations, UpdateTenantOverrides,
};
use crate::domain::{SupportedLocaleRepository, Translator};

#[derive(Clone)]
pub struct LocalizationState {
    /// The negotiation middleware resolver.
    pub resolver: Arc<dyn LocaleResolver>,
    /// The shared service (merged catalogs, cache, metrics).
    pub service: Arc<LocalizationService>,
    /// The synchronous translator (render hot path).
    pub translator: Arc<dyn Translator>,
    /// Localizes problem+json titles.
    pub problem_translator: Arc<dyn ProblemTranslator>,
    /// Catalog delivery for the frontend messages endpoint.
    pub catalog: Arc<CatalogService>,
    pub supported: Arc<dyn SupportedLocaleRepository>,
    pub pool: PgPool,

    pub get_settings: Arc<GetLocalizationSettings>,
    pub update_settings: Arc<UpdateLocalizationSettings>,
    pub get_overrides: Arc<GetTenantOverrides>,
    pub update_overrides: Arc<UpdateTenantOverrides>,
    pub get_platform_translations: Arc<GetPlatformTranslations>,
    pub update_platform_translations: Arc<UpdatePlatformTranslations>,
}
