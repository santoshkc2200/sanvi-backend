use std::sync::Arc;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::UserId;
use sanvi_persistence::TxManager;

use crate::application::storage;
use crate::domain::events::AccessEvent;
use crate::domain::ports::EventPublisher;
use crate::domain::{
    AccessError, Clock, Feature, FeatureKey, FeatureKind, FeatureRepository, Visibility,
};

pub struct DefineFeatureCommand {
    pub key: FeatureKey,
    pub name: String,
    pub kind: FeatureKind,
    pub default_enabled: bool,
    pub default_limit: Option<i64>,
    pub visibility: Visibility,
}

pub struct DefineFeature {
    tx_manager: TxManager,
    features: Arc<dyn FeatureRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl DefineFeature {
    pub fn new(
        tx_manager: TxManager,
        features: Arc<dyn FeatureRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            features,
            events,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        cmd: DefineFeatureCommand,
    ) -> Result<Feature, AccessError> {
        let feature = Feature::new(
            cmd.key.clone(),
            cmd.name,
            cmd.kind,
            cmd.default_enabled,
            cmd.default_limit,
            cmd.visibility,
        )?;

        let mut scope = self
            .tx_manager
            .begin_unscoped("define_feature", "platform-admin")
            .await
            .map_err(storage)?;
        if self.features.find(&mut scope, &cmd.key).await?.is_some() {
            return Err(AccessError::FeatureAlreadyExists {
                key: cmd.key.to_string(),
            });
        }
        self.features.save(&mut scope, &feature).await?;
        self.events
            .publish(
                &mut scope,
                &[AccessEvent::FeatureDefined {
                    key: cmd.key.to_string(),
                }],
            )
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::PlatformAdmin,
                    actor_id: Some(actor),
                    on_behalf_of,
                    tenant_id: None,
                    action: "access.feature_defined".to_string(),
                    resource_type: Some("feature".to_string()),
                    resource_id: Some(cmd.key.to_string()),
                    before: None,
                    after: Some(serde_json::json!({
                        "key": cmd.key.as_str(),
                        "kind": cmd.kind.as_str(),
                        "default_enabled": cmd.default_enabled,
                        "default_limit": cmd.default_limit,
                        "visibility": cmd.visibility.as_str(),
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;
        Ok(feature)
    }
}

pub struct UpdateFeatureCommand {
    pub key: FeatureKey,
    pub name: String,
    pub default_enabled: bool,
    pub default_limit: Option<i64>,
    pub visibility: Visibility,
}

pub struct UpdateFeature {
    tx_manager: TxManager,
    features: Arc<dyn FeatureRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl UpdateFeature {
    pub fn new(
        tx_manager: TxManager,
        features: Arc<dyn FeatureRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            features,
            events,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        cmd: UpdateFeatureCommand,
    ) -> Result<Feature, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("update_feature", "platform-admin")
            .await
            .map_err(storage)?;
        let mut feature = self
            .features
            .find(&mut scope, &cmd.key)
            .await?
            .ok_or_else(|| AccessError::FeatureNotFound {
                key: cmd.key.to_string(),
            })?;
        if feature.is_deprecated() {
            return Err(AccessError::InvalidFeatureDefinition(format!(
                "feature {} is deprecated; changes are not allowed",
                cmd.key
            )));
        }
        let before = serde_json::json!({
            "name": feature.name(),
            "default_enabled": feature.default_enabled(),
            "default_limit": feature.default_limit(),
            "visibility": feature.visibility().as_str(),
        });
        feature.update_defaults(
            cmd.name,
            cmd.default_enabled,
            cmd.default_limit,
            cmd.visibility,
        )?;
        self.features.save(&mut scope, &feature).await?;
        self.events
            .publish(
                &mut scope,
                &[AccessEvent::FeatureDefined {
                    key: cmd.key.to_string(),
                }],
            )
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::PlatformAdmin,
                    actor_id: Some(actor),
                    on_behalf_of,
                    tenant_id: None,
                    action: "access.feature_updated".to_string(),
                    resource_type: Some("feature".to_string()),
                    resource_id: Some(cmd.key.to_string()),
                    before: Some(before),
                    after: Some(serde_json::json!({
                        "name": feature.name(),
                        "default_enabled": feature.default_enabled(),
                        "default_limit": feature.default_limit(),
                        "visibility": feature.visibility().as_str(),
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;
        Ok(feature)
    }
}

pub struct DeprecateFeature {
    tx_manager: TxManager,
    features: Arc<dyn FeatureRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl DeprecateFeature {
    pub fn new(
        tx_manager: TxManager,
        features: Arc<dyn FeatureRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            features,
            events,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        key: &FeatureKey,
    ) -> Result<Feature, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("deprecate_feature", "platform-admin")
            .await
            .map_err(storage)?;
        let mut feature = self.features.find(&mut scope, key).await?.ok_or_else(|| {
            AccessError::FeatureNotFound {
                key: key.to_string(),
            }
        })?;
        feature.deprecate(self.clock.now())?;
        self.features.save(&mut scope, &feature).await?;
        self.events
            .publish(
                &mut scope,
                &[AccessEvent::FeatureDefined {
                    key: key.to_string(),
                }],
            )
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::PlatformAdmin,
                    actor_id: Some(actor),
                    on_behalf_of,
                    tenant_id: None,
                    action: "access.feature_deprecated".to_string(),
                    resource_type: Some("feature".to_string()),
                    resource_id: Some(key.to_string()),
                    before: None,
                    after: None,
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;
        Ok(feature)
    }
}

pub struct ListFeatures {
    tx_manager: TxManager,
    features: Arc<dyn FeatureRepository>,
}

impl ListFeatures {
    pub fn new(tx_manager: TxManager, features: Arc<dyn FeatureRepository>) -> Self {
        Self {
            tx_manager,
            features,
        }
    }

    pub async fn execute(&self) -> Result<Vec<Feature>, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_features", "platform-admin")
            .await
            .map_err(storage)?;
        let features = self.features.list_all(&mut scope).await?;
        scope.commit().await.map_err(storage)?;
        Ok(features)
    }
}
