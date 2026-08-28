//! Composition-root helper for localized outbound notifications.
//!
//! Context notification ports stay independent of localization. API and
//! worker adapters use this helper to resolve a tenant's default locale and
//! timezone, warm override-aware catalogs, and render user-visible copy.

use std::sync::Arc;

use sanvi_persistence::{PgPool, TxManager};
use sanvi_kernel::TenantId;

use crate::LocalizationConfig;
use crate::adapters::{LocalizationService, SqlxTenantLocaleRepository};
use crate::domain::{Arg, Locale, TenantLocaleRepository, Translator};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationLocale {
    pub scope: String,
    pub locale: String,
    pub timezone: String,
}

#[derive(Clone)]
pub struct NotificationLocalizer {
    tx_manager: TxManager,
    service: Arc<LocalizationService>,
    config: LocalizationConfig,
}

impl NotificationLocalizer {
    pub fn new(
        pool: PgPool,
        ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
        service: Arc<LocalizationService>,
        config: LocalizationConfig,
    ) -> Self {
        Self {
            tx_manager: TxManager::new(pool, ids),
            service,
            config,
        }
    }

    pub async fn tenant_locale(&self, tenant_id: TenantId) -> NotificationLocale {
        let fallback_locale = Locale::parse(&self.config.default_locale).unwrap_or_default();
        let mut resolved = NotificationLocale {
            scope: tenant_id.to_string(),
            locale: fallback_locale.as_str().to_string(),
            timezone: default_timezone(fallback_locale.as_str()).to_string(),
        };

        if let Ok(mut scope) = self.tx_manager.begin_for(tenant_id.as_i64()).await {
            let repo = SqlxTenantLocaleRepository::new();
            if let Ok(Some(settings)) = repo.get(&mut scope, tenant_id).await {
                resolved.locale = settings.default_locale.as_str().to_string();
                resolved.timezone = settings.timezone;
            }
            let _ = scope.rollback().await;
        }
        self.warm(&resolved).await;
        resolved
    }

    pub async fn platform_locale(&self) -> NotificationLocale {
        let locale = Locale::parse(&self.config.default_locale).unwrap_or_default();
        let resolved = NotificationLocale {
            scope: "platform".to_string(),
            locale: locale.as_str().to_string(),
            timezone: default_timezone(locale.as_str()).to_string(),
        };
        self.warm(&resolved).await;
        resolved
    }

    pub fn render(&self, context: &NotificationLocale, key: &str, args: &[(&str, Arg)]) -> String {
        self.service.translate_with_timezone(
            &context.scope,
            &context.locale,
            &context.timezone,
            key,
            args,
        )
    }

    async fn warm(&self, context: &NotificationLocale) {
        if let Err(err) = self.service.warm(&context.scope, &context.locale).await {
            tracing::warn!(
                error = %err,
                scope = %context.scope,
                locale = %context.locale,
                "failed to warm notification catalog; static fallback will be used"
            );
        }
        if context.locale != "en" {
            let _ = self.service.warm(&context.scope, "en").await;
        }
    }
}

fn default_timezone(locale: &str) -> &'static str {
    if locale.starts_with("ja") {
        "Asia/Tokyo"
    } else {
        "UTC"
    }
}
