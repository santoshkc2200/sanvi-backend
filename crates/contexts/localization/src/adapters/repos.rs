//! SQLx repositories for the localization context.
//!
//! `tenant_locales` is tenant-scoped (RLS FORCE): every access is reached
//! through a `ScopedTx`, so the tenant isolation holds twice over (type
//! system + Postgres). `translations` mixes scopes: tenant rows obey tenant
//! RLS; platform rows are public to read (they are public copy) and
//! writable only from the audited platform scope.

use sanvi_persistence::{PgPool, PlatformScope, ScopedTx};
use sqlx::FromRow;
use uuid::Uuid;

use crate::domain::catalog::Translation;
use crate::domain::error::LocalizationError;
use crate::domain::locale::Locale;
use crate::domain::ports::{
    SupportedLocale, SupportedLocaleRepository, TenantLocaleRepository, TenantLocaleSettings,
    TranslationRepository, UserLocaleProvider,
};

/// Reads a user's stored locale preference straight from the pool
/// (`identity.users` is platform-scoped, no RLS). The resolver pairs this
/// with a short-TTL cache so the read is not on every request.
#[derive(Debug, Clone)]
pub struct PoolUserLocaleProvider {
    pool: PgPool,
}

impl PoolUserLocaleProvider {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl UserLocaleProvider for PoolUserLocaleProvider {
    async fn locale_for(&self, user_id: Uuid) -> Option<String> {
        let row: Option<Option<String>> =
            sqlx::query_scalar("SELECT locale FROM identity.users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await
                .ok()
                .flatten();
        row.flatten()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxSupportedLocaleRepository;

impl SqlxSupportedLocaleRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl SupportedLocaleRepository for SqlxSupportedLocaleRepository {
    async fn list(&self, pool: &PgPool) -> Result<Vec<SupportedLocale>, LocalizationError> {
        #[derive(FromRow)]
        struct Row {
            code: String,
            name_native: String,
            name_en: String,
            sort_order: i32,
        }
        let rows = sqlx::query_as!(
            Row,
            "SELECT code, name_native, name_en, sort_order
             FROM localization.supported_locales
             WHERE enabled
             ORDER BY sort_order"
        )
        .fetch_all(pool)
        .await
        .map_err(|err| LocalizationError::Storage(err.to_string()))?;

        rows.into_iter()
            .map(|row| {
                Ok(SupportedLocale {
                    code: Locale::parse(&row.code)?,
                    name_native: row.name_native,
                    name_en: row.name_en,
                    sort_order: row.sort_order,
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxTenantLocaleRepository;

impl SqlxTenantLocaleRepository {
    pub fn new() -> Self {
        Self
    }
}

#[derive(FromRow)]
struct TenantLocaleRow {
    default_locale: String,
    enabled_locales: Vec<String>,
    timezone: String,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl TenantLocaleRow {
    fn into_settings(self, tenant_id: Uuid) -> Result<TenantLocaleSettings, LocalizationError> {
        let default_locale = Locale::parse(&self.default_locale)?;
        let enabled_locales = self
            .enabled_locales
            .iter()
            .map(|tag| Locale::parse(tag))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(TenantLocaleSettings {
            tenant_id,
            default_locale,
            enabled_locales,
            timezone: self.timezone,
            updated_at: self.updated_at,
        })
    }
}

#[async_trait::async_trait]
impl TenantLocaleRepository for SqlxTenantLocaleRepository {
    async fn get(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Option<TenantLocaleSettings>, LocalizationError> {
        let row = sqlx::query_as!(
            TenantLocaleRow,
            "SELECT default_locale, enabled_locales, timezone, updated_at
             FROM localization.tenant_locales
             WHERE tenant_id = $1",
            tenant_id
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| LocalizationError::Storage(err.to_string()))?;
        row.map(|row| row.into_settings(tenant_id)).transpose()
    }

    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        settings: &TenantLocaleSettings,
    ) -> Result<(), LocalizationError> {
        let enabled: Vec<String> = settings
            .enabled_locales
            .iter()
            .map(|locale| locale.as_str().to_string())
            .collect();
        sqlx::query!(
            "INSERT INTO localization.tenant_locales
                 (tenant_id, default_locale, enabled_locales, timezone, updated_at)
             VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (tenant_id) DO UPDATE
                 SET default_locale = EXCLUDED.default_locale,
                     enabled_locales = EXCLUDED.enabled_locales,
                     timezone = EXCLUDED.timezone,
                     updated_at = now()",
            settings.tenant_id,
            settings.default_locale.as_str(),
            &enabled,
            settings.timezone,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| LocalizationError::Storage(err.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxTranslationRepository;

impl SqlxTranslationRepository {
    pub fn new() -> Self {
        Self
    }
}

#[derive(FromRow)]
struct TranslationRow {
    scope: String,
    key: String,
    locale: String,
    value: String,
    updated_by: Option<Uuid>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl TranslationRow {
    fn into_translation(self) -> Translation {
        Translation {
            scope: self.scope,
            key: self.key,
            locale: self.locale,
            value: self.value,
            updated_by: self.updated_by,
            updated_at: self.updated_at,
        }
    }
}

#[async_trait::async_trait]
impl TranslationRepository for SqlxTranslationRepository {
    async fn list_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<Translation>, LocalizationError> {
        let rows = sqlx::query_as!(
            TranslationRow,
            "SELECT scope, key, locale, value, updated_by, updated_at
             FROM localization.translations
             WHERE scope = $1",
            tenant_id.to_string()
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| LocalizationError::Storage(err.to_string()))?;
        Ok(rows
            .into_iter()
            .map(TranslationRow::into_translation)
            .collect())
    }

    async fn list_platform(&self, pool: &PgPool) -> Result<Vec<Translation>, LocalizationError> {
        let rows = sqlx::query_as!(
            TranslationRow,
            "SELECT scope, key, locale, value, updated_by, updated_at
             FROM localization.translations
             WHERE scope = 'platform'"
        )
        .fetch_all(pool)
        .await
        .map_err(|err| LocalizationError::Storage(err.to_string()))?;
        Ok(rows
            .into_iter()
            .map(TranslationRow::into_translation)
            .collect())
    }

    async fn upsert_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        translation: &Translation,
    ) -> Result<(), LocalizationError> {
        sqlx::query!(
            "INSERT INTO localization.translations
                 (scope, key, locale, value, updated_by, updated_at)
             VALUES ($1, $2, $3, $4, $5, now())
             ON CONFLICT (scope, key, locale) DO UPDATE
                 SET value = EXCLUDED.value,
                     updated_by = EXCLUDED.updated_by,
                     updated_at = now()",
            tenant_id.to_string(),
            translation.key,
            translation.locale,
            translation.value,
            translation.updated_by,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| LocalizationError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn upsert_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        translation: &Translation,
    ) -> Result<(), LocalizationError> {
        sqlx::query!(
            "INSERT INTO localization.translations
                 (scope, key, locale, value, updated_by, updated_at)
             VALUES ('platform', $1, $2, $3, $4, now())
             ON CONFLICT (scope, key, locale) DO UPDATE
                 SET value = EXCLUDED.value,
                     updated_by = EXCLUDED.updated_by,
                     updated_at = now()",
            translation.key,
            translation.locale,
            translation.value,
            translation.updated_by,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| LocalizationError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn delete_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        key: &str,
        locale: &str,
    ) -> Result<(), LocalizationError> {
        sqlx::query!(
            "DELETE FROM localization.translations
             WHERE scope = $1 AND key = $2 AND locale = $3",
            tenant_id.to_string(),
            key,
            locale,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| LocalizationError::Storage(err.to_string()))?;
        Ok(())
    }
}
