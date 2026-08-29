//! Tenant localization settings: default locale, enabled locales, timezone.

use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::{PgPool, ScopedTx};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::error::LocalizationError;
use crate::domain::events::LocalizationEvent;
use crate::domain::locale::Locale;
use crate::domain::ports::{
    Clock, EventPublisher, SupportedLocaleRepository, TenantLocaleRepository, TenantLocaleSettings,
};

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct LocalizationSettingsView {
    pub default_locale: String,
    pub enabled_locales: Vec<String>,
    pub timezone: String,
    pub updated_at: DateTime<Utc>,
}

impl From<TenantLocaleSettings> for LocalizationSettingsView {
    fn from(settings: TenantLocaleSettings) -> Self {
        Self {
            default_locale: settings.default_locale.as_str().to_string(),
            enabled_locales: settings
                .enabled_locales
                .iter()
                .map(|locale| locale.as_str().to_string())
                .collect(),
            timezone: settings.timezone,
            updated_at: settings.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UpdateLocalizationSettingsCommand {
    pub default_locale: String,
    pub enabled_locales: Vec<String>,
    pub timezone: String,
    /// The locale the change was made under (the admin's negotiated
    /// locale), carried on the event so downstream work renders correctly.
    #[serde(default = "default_acting_locale")]
    pub acting_locale: String,
}

fn default_acting_locale() -> String {
    Locale::DEFAULT.to_string()
}

pub struct GetLocalizationSettings {
    repo: Arc<dyn TenantLocaleRepository>,
}

impl GetLocalizationSettings {
    pub fn new(repo: Arc<dyn TenantLocaleRepository>) -> Self {
        Self { repo }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<LocalizationSettingsView, LocalizationError> {
        match self.repo.get(scope, tenant_id).await? {
            Some(settings) => Ok(settings.into()),
            None => Ok(TenantLocaleSettings::default_for(tenant_id).into()),
        }
    }
}

pub struct UpdateLocalizationSettings {
    repo: Arc<dyn TenantLocaleRepository>,
    supported: Arc<dyn SupportedLocaleRepository>,
    pool: PgPool,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl UpdateLocalizationSettings {
    pub fn new(
        repo: Arc<dyn TenantLocaleRepository>,
        supported: Arc<dyn SupportedLocaleRepository>,
        pool: PgPool,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            repo,
            supported,
            pool,
            events,
            clock,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        command: &UpdateLocalizationSettingsCommand,
    ) -> Result<LocalizationSettingsView, LocalizationError> {
        // Every enabled locale must be platform-supported; the default must
        // be enabled; the timezone must parse.
        let platform = self.supported.list(&self.pool).await?;
        let platform_tags: Vec<String> = platform
            .iter()
            .map(|entry| entry.code.as_str().to_string())
            .collect();

        let enabled: Vec<Locale> = command
            .enabled_locales
            .iter()
            .map(|tag| Locale::parse(tag))
            .collect::<Result<_, _>>()?;
        if enabled.is_empty() {
            return Err(LocalizationError::SettingsConflict(
                "at least one enabled locale is required".to_string(),
            ));
        }
        for locale in &enabled {
            if !platform_tags.iter().any(|tag| tag == locale.as_str()) {
                return Err(LocalizationError::UnsupportedLocale(
                    locale.as_str().to_string(),
                ));
            }
        }
        let default = Locale::parse(&command.default_locale)?;
        if !enabled.contains(&default) {
            return Err(LocalizationError::SettingsConflict(format!(
                "default locale {default} is not in the enabled set"
            )));
        }
        if chrono_tz::Tz::from_str(&command.timezone).is_err() {
            return Err(LocalizationError::InvalidTimezone(command.timezone.clone()));
        }
        let acting_locale = Locale::parse(&command.acting_locale)?;

        let settings = TenantLocaleSettings {
            tenant_id,
            default_locale: default.clone(),
            enabled_locales: enabled.clone(),
            timezone: command.timezone.clone(),
            updated_at: self.clock.now(),
        };
        self.repo.upsert(scope, &settings).await?;

        let event = LocalizationEvent::TenantLocalesChanged {
            tenant_id,
            default_locale: default.as_str().to_string(),
            enabled_locales: enabled
                .iter()
                .map(|locale| locale.as_str().to_string())
                .collect(),
            timezone: command.timezone.clone(),
            locale: acting_locale.as_str().to_string(),
        };
        self.events.publish_scoped(scope, &event).await?;

        Ok(settings.into())
    }
}
