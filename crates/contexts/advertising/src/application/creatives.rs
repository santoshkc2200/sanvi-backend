//! Creative management: create with upload-time matrix validation, get, list,
//! delete, and placement previews.

use std::sync::Arc;

use chrono::Utc;
use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::{
    application::PlatformRegistry,
    domain::{
        AdvertisingConnectionRepository, AdvertisingError, AssetMetadata, ConnectionStatus,
        Creative, CreativeAssetSpec, CreativeRepository, CreativeText, StoredCreative,
        ValidationViolation,
    },
};

/// Fields for creating a new creative. Validated at create/upload time against
/// the platform's capability matrix.
#[derive(Debug, Clone)]
pub struct NewCreativeFields {
    pub connection_id: Uuid,
    pub placement: String,
    pub texts: Vec<CreativeText>,
    pub asset_references: Vec<String>,
    pub assets: Vec<AssetMetadata>,
}

pub struct CreateCreative {
    tx: TxManager,
    creatives: Arc<dyn CreativeRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    platforms: Arc<PlatformRegistry>,
}

impl CreateCreative {
    pub fn new(
        tx: TxManager,
        creatives: Arc<dyn CreativeRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        platforms: Arc<PlatformRegistry>,
    ) -> Self {
        Self {
            tx,
            creatives,
            connections,
            platforms,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: Uuid,
        fields: NewCreativeFields,
    ) -> Result<StoredCreative, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let connection = self
            .connections
            .find_by_id(&mut scope, tenant_id, fields.connection_id)
            .await?
            .ok_or(AdvertisingError::ConnectionNotFound)?;

        if connection.status() != ConnectionStatus::Active {
            return Err(AdvertisingError::ConnectionNotActive);
        }

        let adapter = self.platforms.adapter(connection.platform())?;
        let matrix = &adapter.descriptor().capability_matrix;

        let mut violations = Vec::new();
        let placement_spec = matrix.placement(&fields.placement);
        if placement_spec.is_none() {
            violations.push(ValidationViolation::unsupported(
                "placement",
                "creative placement is unavailable for this platform",
            ));
        }

        violations.extend(matrix.validate_creative_text(&fields.texts));

        if let Some(placement) = placement_spec {
            if placement.asset_spec.image_required && fields.assets.is_empty() {
                violations.push(ValidationViolation::invalid(
                    "assets",
                    "at least one asset is required for this placement",
                ));
            }
            for (index, asset) in fields.assets.iter().enumerate() {
                let asset_violations = placement.asset_spec.validate_asset(asset);
                for violation in asset_violations {
                    violations.push(ValidationViolation {
                        field_path: format!("assets[{index}].{}", violation.field_path),
                        code: violation.code,
                        message: violation.message,
                    });
                }
            }
        }

        if !violations.is_empty() {
            return Err(AdvertisingError::Validation(violations));
        }

        let id = Uuid::now_v7();
        let stored = StoredCreative {
            id,
            tenant_id,
            connection_id: fields.connection_id,
            platform: connection.platform().clone(),
            external_id: None,
            creative: Creative {
                id: id.to_string(),
                placement: fields.placement,
                texts: fields.texts,
                asset_references: fields.asset_references,
            },
            assets: fields.assets,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        self.creatives.insert(&mut scope, &stored).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(stored)
    }
}

pub struct GetCreative {
    tx: TxManager,
    creatives: Arc<dyn CreativeRepository>,
}

impl GetCreative {
    pub fn new(tx: TxManager, creatives: Arc<dyn CreativeRepository>) -> Self {
        Self { tx, creatives }
    }

    pub async fn execute(
        &self,
        tenant_id: Uuid,
        creative_id: Uuid,
    ) -> Result<Option<StoredCreative>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.creatives
            .find(&mut scope, tenant_id, creative_id)
            .await
    }
}

pub struct ListCreatives {
    tx: TxManager,
    creatives: Arc<dyn CreativeRepository>,
}

impl ListCreatives {
    pub fn new(tx: TxManager, creatives: Arc<dyn CreativeRepository>) -> Self {
        Self { tx, creatives }
    }

    pub async fn execute(&self, tenant_id: Uuid) -> Result<Vec<StoredCreative>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.creatives.list_by_tenant(&mut scope, tenant_id).await
    }
}

pub struct DeleteCreative {
    tx: TxManager,
    creatives: Arc<dyn CreativeRepository>,
}

impl DeleteCreative {
    pub fn new(tx: TxManager, creatives: Arc<dyn CreativeRepository>) -> Self {
        Self { tx, creatives }
    }

    pub async fn execute(
        &self,
        tenant_id: Uuid,
        creative_id: Uuid,
    ) -> Result<(), AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let deleted = self
            .creatives
            .delete(&mut scope, tenant_id, creative_id)
            .await?;
        if !deleted {
            return Err(AdvertisingError::CreativeNotFound);
        }
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PlacementPreview {
    pub placement: String,
    pub headline: Option<String>,
    pub body: Option<String>,
    pub asset_references: Vec<String>,
    pub spec: CreativeAssetSpec,
}

pub struct GetCreativePreviews {
    tx: TxManager,
    creatives: Arc<dyn CreativeRepository>,
    platforms: Arc<PlatformRegistry>,
}

impl GetCreativePreviews {
    pub fn new(
        tx: TxManager,
        creatives: Arc<dyn CreativeRepository>,
        platforms: Arc<PlatformRegistry>,
    ) -> Self {
        Self {
            tx,
            creatives,
            platforms,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: Uuid,
        creative_id: Uuid,
        placement_filter: Option<String>,
    ) -> Result<Vec<PlacementPreview>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let stored = self
            .creatives
            .find(&mut scope, tenant_id, creative_id)
            .await?
            .ok_or(AdvertisingError::CreativeNotFound)?;

        let adapter = self.platforms.adapter(&stored.platform)?;
        let matrix = &adapter.descriptor().capability_matrix;

        let matching_placements: Vec<_> = matrix
            .creative_placements
            .iter()
            .filter(|placement| {
                if let Some(filter) = &placement_filter {
                    &placement.key == filter
                } else {
                    placement.key == stored.creative.placement
                }
            })
            .collect();

        let default_text = stored
            .creative
            .texts
            .iter()
            .find(|text| text.locale == "default")
            .or_else(|| stored.creative.texts.first());

        let previews = matching_placements
            .into_iter()
            .map(|placement| PlacementPreview {
                placement: placement.key.clone(),
                headline: default_text.map(|t| t.headline.clone()),
                body: default_text.map(|t| t.body.clone()),
                asset_references: stored.creative.asset_references.clone(),
                spec: placement.asset_spec.clone(),
            })
            .collect();

        Ok(previews)
    }
}
