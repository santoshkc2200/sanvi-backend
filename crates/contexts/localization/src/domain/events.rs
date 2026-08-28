//! Localization domain events.
//!
//! Every event that can trigger a user-visible render downstream carries
//! the locale it happened under via `EventEnvelope::with_locale` — workers
//! never re-negotiate a locale from a missing request.

use sanvi_kernel::{EventEnvelope, EventId, SnowflakeId, TenantId};
use serde::Serialize;

use crate::domain::locale::Locale;

pub const TENANT_LOCALES_AGGREGATE: &str = "localization.tenant_locales";
pub const TRANSLATION_AGGREGATE: &str = "localization.translation";
pub const CATALOG_AGGREGATE: &str = "localization.catalog";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalizationEventName {
    TenantLocalesChanged,
    TranslationUpdated,
    TranslationDeleted,
    CatalogPublished,
}

impl LocalizationEventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TenantLocalesChanged => "localization.tenant_locales_changed",
            Self::TranslationUpdated => "localization.translation_updated",
            Self::TranslationDeleted => "localization.translation_deleted",
            Self::CatalogPublished => "localization.catalog_published",
        }
    }
}

/// The events the localization context publishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocalizationEvent {
    /// A tenant changed its default locale, enabled locales or timezone.
    TenantLocalesChanged {
        tenant_id: TenantId,
        default_locale: String,
        enabled_locales: Vec<String>,
        timezone: String,
        /// The locale the change was made under (the admin's negotiated
        /// locale) — carried into async work.
        locale: String,
    },
    /// A copy override was updated (tenant or platform scope).
    TranslationUpdated {
        scope: String,
        key: String,
        locale: String,
        value: String,
        /// The locale the change was made under.
        acting_locale: String,
    },
    /// A copy override was removed by replacement-style PUT semantics.
    TranslationDeleted {
        scope: String,
        key: String,
        locale: String,
        acting_locale: String,
    },
    /// The static catalog was released (informational).
    CatalogPublished { version: String },
}

impl LocalizationEvent {
    pub fn name(&self) -> LocalizationEventName {
        match self {
            Self::TenantLocalesChanged { .. } => LocalizationEventName::TenantLocalesChanged,
            Self::TranslationUpdated { .. } => LocalizationEventName::TranslationUpdated,
            Self::TranslationDeleted { .. } => LocalizationEventName::TranslationDeleted,
            Self::CatalogPublished { .. } => LocalizationEventName::CatalogPublished,
        }
    }

    /// The event's aggregate id. Tenant-scoped events use the tenant's
    /// Snowflake; scope-keyed translation events parse `scope` as a tenant
    /// Snowflake when it names one. Events with no natural first-party
    /// aggregate (a platform-scoped translation or catalog publish) fall
    /// back to the event's own id, mirroring the privacy context's pattern.
    pub fn aggregate_id(&self, id: EventId) -> SnowflakeId {
        match self {
            Self::TenantLocalesChanged { tenant_id, .. } => tenant_id.as_snowflake(),
            Self::TranslationUpdated { scope, .. } | Self::TranslationDeleted { scope, .. } => {
                scope
                    .parse::<TenantId>()
                    .map(|tenant_id| tenant_id.as_snowflake())
                    .unwrap_or_else(|_| id.as_snowflake())
            }
            Self::CatalogPublished { .. } => id.as_snowflake(),
        }
    }

    pub fn envelope(&self, id: EventId) -> EventEnvelope {
        let aggregate = match self {
            Self::TenantLocalesChanged { .. } => TENANT_LOCALES_AGGREGATE,
            Self::TranslationUpdated { .. } => TRANSLATION_AGGREGATE,
            Self::TranslationDeleted { .. } => TRANSLATION_AGGREGATE,
            Self::CatalogPublished { .. } => CATALOG_AGGREGATE,
        };
        EventEnvelope::new(
            id,
            aggregate,
            self.aggregate_id(id),
            self.name().as_str(),
            serde_json::to_value(self).expect("localization events serialize"),
        )
        .with_metadata(
            "tenant_id",
            serde_json::json!(match self {
                Self::TenantLocalesChanged { tenant_id, .. } => tenant_id.to_string(),
                Self::TranslationUpdated { scope, .. } => scope.clone(),
                Self::TranslationDeleted { scope, .. } => scope.clone(),
                Self::CatalogPublished { .. } => "platform".to_string(),
            }),
        )
        .with_locale(match self {
            Self::TenantLocalesChanged { locale, .. }
            | Self::TranslationUpdated {
                acting_locale: locale,
                ..
            }
            | Self::TranslationDeleted {
                acting_locale: locale,
                ..
            } => locale.clone(),
            Self::CatalogPublished { .. } => Locale::DEFAULT.to_string(),
        })
    }
}
