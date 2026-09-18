//! Copy overrides: tenant tone-of-voice and platform catalog overrides.
//!
//! PUT semantics: the provided set replaces the scope's overrides. Every
//! provided entry is validated first (key must exist in the source catalog,
//! pattern must parse, locale must be well-formed), so a typo'd key can
//! never silently ship.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{PgPool, PlatformScope, ScopedTx};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::catalog::Translation;
use crate::domain::error::LocalizationError;
use crate::domain::events::LocalizationEvent;
use crate::domain::locale::Locale;
use crate::domain::ports::{Clock, EventPublisher, TranslationRepository};

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct TranslationView {
    pub scope: String,
    pub key: String,
    pub locale: String,
    pub value: String,
    pub updated_at: DateTime<Utc>,
}

impl From<Translation> for TranslationView {
    fn from(translation: Translation) -> Self {
        Self {
            scope: translation.scope,
            key: translation.key,
            locale: translation.locale,
            value: translation.value,
            updated_at: translation.updated_at,
        }
    }
}

/// One override in a PUT body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TranslationPatch {
    pub key: String,
    pub locale: String,
    pub value: String,
}

pub struct GetTenantOverrides {
    repo: Arc<dyn TranslationRepository>,
}

impl GetTenantOverrides {
    pub fn new(repo: Arc<dyn TranslationRepository>) -> Self {
        Self { repo }
    }

    #[tracing::instrument(name = "localization.get_tenant_overrides", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<TranslationView>, LocalizationError> {
        let overrides = self.repo.list_tenant(scope, tenant_id).await?;
        Ok(overrides.into_iter().map(TranslationView::from).collect())
    }
}

pub struct UpdateTenantOverrides {
    repo: Arc<dyn TranslationRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl UpdateTenantOverrides {
    pub fn new(
        repo: Arc<dyn TranslationRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            repo,
            events,
            clock,
        }
    }

    #[tracing::instrument(name = "localization.update_tenant_overrides", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        actor_id: Option<UserId>,
        acting_locale: &str,
        patches: &[TranslationPatch],
    ) -> Result<Vec<TranslationView>, LocalizationError> {
        let acting_locale = Locale::parse(acting_locale)?;

        // Validate everything first: no partial writes.
        for patch in patches {
            let locale = Locale::parse(&patch.locale)?;
            let translation = Translation {
                scope: tenant_id.to_string(),
                key: patch.key.clone(),
                locale: locale.as_str().to_string(),
                value: patch.value.clone(),
                updated_by: actor_id,
                updated_at: self.clock.now(),
            };
            translation.validate()?;
        }

        let existing = self.repo.list_tenant(scope, tenant_id).await?;

        // Replace semantics: drop rows that disappeared from the patch set.
        for previous in &existing {
            let still_present = patches
                .iter()
                .any(|patch| patch.key == previous.key && patch.locale == previous.locale);
            if !still_present {
                self.repo
                    .delete_tenant(scope, tenant_id, &previous.key, &previous.locale)
                    .await?;
                self.events
                    .publish_scoped(
                        scope,
                        &LocalizationEvent::TranslationDeleted {
                            scope: tenant_id.to_string(),
                            key: previous.key.clone(),
                            locale: previous.locale.clone(),
                            acting_locale: acting_locale.as_str().to_string(),
                        },
                    )
                    .await?;
            }
        }

        // Upsert the new set and publish one event per change.
        let mut updated = Vec::new();
        for patch in patches {
            let locale = Locale::parse(&patch.locale)?;
            let translation = Translation {
                scope: tenant_id.to_string(),
                key: patch.key.clone(),
                locale: locale.as_str().to_string(),
                value: patch.value.clone(),
                updated_by: actor_id,
                updated_at: self.clock.now(),
            };
            self.repo
                .upsert_tenant(scope, tenant_id, &translation)
                .await?;
            self.events
                .publish_scoped(
                    scope,
                    &LocalizationEvent::TranslationUpdated {
                        scope: tenant_id.to_string(),
                        key: translation.key.clone(),
                        locale: translation.locale.clone(),
                        value: translation.value.clone(),
                        acting_locale: acting_locale.as_str().to_string(),
                    },
                )
                .await?;
            updated.push(TranslationView::from(translation));
        }

        Ok(updated)
    }
}

pub struct GetPlatformTranslations {
    repo: Arc<dyn TranslationRepository>,
    pool: PgPool,
}

impl GetPlatformTranslations {
    pub fn new(repo: Arc<dyn TranslationRepository>, pool: PgPool) -> Self {
        Self { repo, pool }
    }

    #[tracing::instrument(name = "localization.get_platform_translations", skip_all)]
    pub async fn execute(&self) -> Result<Vec<TranslationView>, LocalizationError> {
        let overrides = self.repo.list_platform(&self.pool).await?;
        Ok(overrides.into_iter().map(TranslationView::from).collect())
    }
}

pub struct UpdatePlatformTranslations {
    repo: Arc<dyn TranslationRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl UpdatePlatformTranslations {
    pub fn new(
        repo: Arc<dyn TranslationRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            repo,
            events,
            clock,
        }
    }

    #[tracing::instrument(name = "localization.update_platform_translations", skip_all)]
    pub async fn execute(
        &self,
        scope: &mut PlatformScope<'_>,
        actor_id: Option<UserId>,
        acting_locale: &str,
        patches: &[TranslationPatch],
    ) -> Result<Vec<TranslationView>, LocalizationError> {
        let acting_locale = Locale::parse(acting_locale)?;

        // Validate everything first.
        for patch in patches {
            let locale = Locale::parse(&patch.locale)?;
            Translation {
                scope: "platform".to_string(),
                key: patch.key.clone(),
                locale: locale.as_str().to_string(),
                value: patch.value.clone(),
                updated_by: actor_id,
                updated_at: self.clock.now(),
            }
            .validate()?;
        }

        let existing = self.repo.list_platform_scoped(scope).await?;
        for previous in &existing {
            let still_present = patches
                .iter()
                .any(|patch| patch.key == previous.key && patch.locale == previous.locale);
            if !still_present {
                self.repo
                    .delete_platform(scope, &previous.key, &previous.locale)
                    .await?;
                self.events
                    .publish(
                        scope,
                        &LocalizationEvent::TranslationDeleted {
                            scope: "platform".to_string(),
                            key: previous.key.clone(),
                            locale: previous.locale.clone(),
                            acting_locale: acting_locale.as_str().to_string(),
                        },
                    )
                    .await?;
            }
        }

        let mut updated = Vec::new();
        for patch in patches {
            let locale = Locale::parse(&patch.locale)?;
            let translation = Translation {
                scope: "platform".to_string(),
                key: patch.key.clone(),
                locale: locale.as_str().to_string(),
                value: patch.value.clone(),
                updated_by: actor_id,
                updated_at: self.clock.now(),
            };
            self.repo.upsert_platform(scope, &translation).await?;
            self.events
                .publish(
                    scope,
                    &LocalizationEvent::TranslationUpdated {
                        scope: "platform".to_string(),
                        key: translation.key.clone(),
                        locale: translation.locale.clone(),
                        value: translation.value.clone(),
                        acting_locale: acting_locale.as_str().to_string(),
                    },
                )
                .await?;
            updated.push(TranslationView::from(translation));
        }

        Ok(updated)
    }
}
