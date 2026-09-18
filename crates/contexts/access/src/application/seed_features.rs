//! Idempotent seeding of the compile-time feature catalog. Operators extend
//! the catalog at runtime via the API; the seeds are the keys phases 06–10
//! gate behind entitlements.

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::application::storage;
use crate::domain::{AccessError, FEATURE_SEEDS, Feature, FeatureRepository};

pub struct SeedFeatures {
    tx_manager: TxManager,
    features: Arc<dyn FeatureRepository>,
}

impl SeedFeatures {
    pub fn new(tx_manager: TxManager, features: Arc<dyn FeatureRepository>) -> Self {
        Self {
            tx_manager,
            features,
        }
    }

    /// Insert every seed missing from the catalog. Existing rows are left
    /// untouched (seeds are defaults, not overrides). Returns the number of
    /// rows created.
    #[tracing::instrument(name = "access.seed_features", skip_all)]
    pub async fn execute(&self) -> Result<usize, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("seed_features", "access")
            .await
            .map_err(storage)?;
        let mut created = 0;
        for seed in FEATURE_SEEDS {
            let key = crate::domain::FeatureKey::parse(seed.key)?;
            if self.features.find(&mut scope, &key).await?.is_none() {
                let feature = Feature::new(
                    key,
                    seed.name.to_string(),
                    seed.kind,
                    seed.default_enabled,
                    seed.default_limit,
                    seed.visibility,
                )?;
                self.features.save(&mut scope, &feature).await?;
                created += 1;
            }
        }
        scope.commit().await.map_err(storage)?;
        Ok(created)
    }
}
