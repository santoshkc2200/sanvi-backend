//! Catalog delivery: the merged message set for one (scope, domain, locale)
//! — the body of `GET /api/v1/public/messages/{domain}`.

use std::collections::BTreeMap;
use std::sync::Arc;

use sanvi_persistence::PgPool;
use serde::Serialize;
use utoipa::ToSchema;

use crate::adapters::catalog::LocalizationService;
use crate::domain::{Locale, LocalizationError, SupportedLocale, SupportedLocaleRepository};

/// The catalog the frontend consumes: every message for one domain in one
/// locale, plus a revision (ETag) that changes whenever content does.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct CatalogView {
    pub domain: String,
    pub locale: String,
    pub revision: String,
    pub messages: BTreeMap<String, String>,
}

pub struct CatalogService {
    pool: PgPool,
    service: Arc<LocalizationService>,
    supported: Arc<dyn SupportedLocaleRepository>,
    default_locale: String,
}

impl CatalogService {
    pub fn new(
        pool: PgPool,
        service: Arc<LocalizationService>,
        supported: Arc<dyn SupportedLocaleRepository>,
        default_locale: String,
    ) -> Self {
        Self {
            pool,
            service,
            supported,
            default_locale,
        }
    }

    /// The known static domains (what the binary ships).
    pub fn known_domains() -> &'static [&'static str] {
        &["billing", "emails", "errors", "privacy"]
    }

    /// Build the merged catalog for a domain + locale under one scope.
    pub async fn catalog(
        &self,
        scope: &str,
        domain: &str,
        locale: &str,
    ) -> Result<CatalogView, LocalizationError> {
        if !Self::known_domains().contains(&domain) {
            return Err(LocalizationError::UnknownDomain(domain.to_string()));
        }
        let locale = Locale::parse(locale)?;

        // Warm the (scope, locale) snapshot, then filter to the domain.
        self.service.warm(scope, locale.as_str()).await?;
        let cache = self.service.cache();
        let ttl = std::time::Duration::from_secs(30);
        let merged = match cache.get_merged(scope, locale.as_str(), ttl) {
            Some(merged) => merged,
            None => {
                // Extremely unlikely (warm just succeeded); retry once.
                self.service.warm(scope, locale.as_str()).await?;
                cache
                    .get_merged(scope, locale.as_str(), ttl)
                    .expect("warm followed by cache read cannot miss")
            }
        };

        let messages: BTreeMap<String, String> = merged
            .raw
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{domain}.")))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        let revision = {
            use std::hash::{DefaultHasher, Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            for (key, value) in &messages {
                key.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            format!("{:016x}", hasher.finish())
        };

        Ok(CatalogView {
            domain: domain.to_string(),
            locale: locale.as_str().to_string(),
            revision,
            messages,
        })
    }

    /// The platform default locale.
    pub fn default_locale(&self) -> &str {
        &self.default_locale
    }

    /// The platform's enabled supported locales for `GET /locales`.
    pub async fn supported_locales(&self) -> Result<Vec<SupportedLocale>, LocalizationError> {
        self.supported.list(&self.pool).await
    }
}
