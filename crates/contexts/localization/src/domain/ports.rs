//! The localization context's ports: repositories, outbox publishing, time
//! and the cross-context user-locale hook.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::{PgPool, PlatformScope, ScopedTx};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::catalog::Translation;
use crate::domain::error::LocalizationError;
use crate::domain::events::LocalizationEvent;
use crate::domain::locale::Locale;

/// A supported locale row from `localization.supported_locales`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SupportedLocale {
    pub code: Locale,
    pub name_native: String,
    pub name_en: String,
    pub sort_order: i32,
}

/// The platform-wide set of enabled locales. Read on the negotiation hot
/// path, so implementations must cache (the sqlx adapter is paired with an
/// in-memory cache).
#[async_trait]
pub trait SupportedLocaleRepository: Send + Sync {
    async fn list(&self, pool: &PgPool) -> Result<Vec<SupportedLocale>, LocalizationError>;
}

/// A tenant's locale settings: default, enabled set, IANA timezone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantLocaleSettings {
    pub tenant_id: Uuid,
    pub default_locale: Locale,
    pub enabled_locales: Vec<Locale>,
    pub timezone: String,
    pub updated_at: DateTime<Utc>,
}

impl TenantLocaleSettings {
    pub fn default_for(tenant_id: Uuid) -> Self {
        Self {
            tenant_id,
            default_locale: Locale::default(),
            enabled_locales: vec![Locale::default()],
            timezone: "UTC".to_string(),
            updated_at: Utc::now(),
        }
    }
}

/// Tenant-scoped locale settings (`localization.tenant_locales`, RLS).
#[async_trait]
pub trait TenantLocaleRepository: Send + Sync {
    async fn get(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Option<TenantLocaleSettings>, LocalizationError>;

    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        settings: &TenantLocaleSettings,
    ) -> Result<(), LocalizationError>;
}

/// Runtime copy overrides (`localization.translations`). Tenant rows are
/// read/written inside the tenant RLS scope; platform rows are read by any
/// role (they are public copy) and written only from the audited platform
/// scope.
#[async_trait]
pub trait TranslationRepository: Send + Sync {
    async fn list_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<Translation>, LocalizationError>;

    async fn list_platform(&self, pool: &PgPool) -> Result<Vec<Translation>, LocalizationError>;

    async fn upsert_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        translation: &Translation,
    ) -> Result<(), LocalizationError>;

    async fn upsert_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        translation: &Translation,
    ) -> Result<(), LocalizationError>;

    async fn delete_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        key: &str,
        locale: &str,
    ) -> Result<(), LocalizationError>;
}

/// The cross-context hook for a signed-in user's stored locale preference
/// (implemented by the composition root over the identity context's user
/// repository; the localization context must not depend on identity).
#[async_trait]
pub trait UserLocaleProvider: Send + Sync {
    async fn locale_for(&self, user_id: Uuid) -> Option<String>;
}

/// Source of time, injectable for tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Outbox-backed event publishing (mirrors the other contexts' pattern).
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        event: &LocalizationEvent,
    ) -> Result<(), LocalizationError>;

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        event: &LocalizationEvent,
    ) -> Result<(), LocalizationError>;
}
