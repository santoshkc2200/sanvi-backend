//! The sqlx feature catalog repository (`access.features`). The catalog is
//! global (no RLS); reads/writes run under the audited platform scope.

use async_trait::async_trait;
use sanvi_persistence::PlatformScope;
use sqlx::FromRow;

use crate::domain::features::{Feature, FeatureKey, FeatureKind, Visibility};
use crate::domain::{AccessError, FeatureRepository};

#[derive(Debug, Clone, FromRow)]
struct FeatureRow {
    key: String,
    name: String,
    kind: String,
    default_enabled: bool,
    default_limit: Option<i64>,
    visibility: String,
    deprecated_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn to_feature(row: FeatureRow) -> Result<Feature, AccessError> {
    Ok(Feature::from_parts(
        FeatureKey::parse(&row.key)?,
        row.name,
        FeatureKind::parse(&row.kind)?,
        row.default_enabled,
        row.default_limit,
        Visibility::parse(&row.visibility)?,
        row.deprecated_at,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxFeatureRepository;

impl SqlxFeatureRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FeatureRepository for SqlxFeatureRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        feature: &Feature,
    ) -> Result<(), AccessError> {
        sqlx::query(
            "INSERT INTO access.features
               (key, name, kind, default_enabled, default_limit, visibility, deprecated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (key) DO UPDATE SET
               name = EXCLUDED.name,
               kind = EXCLUDED.kind,
               default_enabled = EXCLUDED.default_enabled,
               default_limit = EXCLUDED.default_limit,
               visibility = EXCLUDED.visibility,
               deprecated_at = EXCLUDED.deprecated_at",
        )
        .bind(feature.key().as_str())
        .bind(feature.name())
        .bind(feature.kind().as_str())
        .bind(feature.default_enabled())
        .bind(feature.default_limit())
        .bind(feature.visibility().as_str())
        .bind(feature.deprecated_at())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        key: &FeatureKey,
    ) -> Result<Option<Feature>, AccessError> {
        let row = sqlx::query_as(
            "SELECT key, name, kind, default_enabled, default_limit, visibility, deprecated_at
             FROM access.features WHERE key = $1",
        )
        .bind(key.as_str())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        row.map(to_feature).transpose()
    }

    async fn list_all(&self, scope: &mut PlatformScope<'_>) -> Result<Vec<Feature>, AccessError> {
        let rows = sqlx::query_as(
            "SELECT key, name, kind, default_enabled, default_limit, visibility, deprecated_at
             FROM access.features ORDER BY key",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        rows.into_iter().map(to_feature).collect()
    }
}
