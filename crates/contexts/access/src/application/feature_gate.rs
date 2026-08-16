//! Entitlement resolution and the `FeatureGate` implementation every other
//! context consumes.
//!
//! Resolution is a pure function (see [`resolve_all`]) over the feature
//! catalog and the tenant's entitlement rows. The result is memoised per
//! tenant in the [`EntitlementCache`] (Redis, TTL 300 s by default) and
//! invalidated on entitlement/feature events by the worker.
//!
//! Failure semantics:
//! - `is_enabled` may serve a cached boolean answer (the short TTL bounds
//!   the staleness window when an invalidation event is lost); it fails
//!   closed when the backend is unreachable and nothing is cached.
//! - Quota `remaining`/`consume` never trust the cache: the atomic counter
//!   in Postgres is authoritative, so quotas can never oversell.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::application::storage;
use crate::domain::events::AccessEvent;
use crate::domain::ports::EventPublisher;
use crate::domain::quota::period_start;
use crate::domain::{
    AccessError, Clock, EntitlementCache, EntitlementRepository, FeatureGate, FeatureKey,
    FeatureKind, FeatureRepository, QuotaUsageStore, ResolvedEntitlement, resolve_all,
};

/// The resolved set a gate check consumes.
#[derive(Debug, Clone)]
pub enum ResolvedSet {
    /// Fresh from the database (or a fresh cache write).
    Fresh(Vec<ResolvedEntitlement>),
    /// Served from the cache (possibly stale by up to the cache TTL).
    Cached(Vec<ResolvedEntitlement>),
}

impl ResolvedSet {
    fn entries(&self) -> &[ResolvedEntitlement] {
        match self {
            Self::Fresh(entries) | Self::Cached(entries) => entries,
        }
    }

    fn find(&self, feature: &FeatureKey) -> Option<&ResolvedEntitlement> {
        self.entries()
            .iter()
            .find(|entry| &entry.feature == feature)
    }
}

/// Resolves a tenant's full entitlement set (cache-first, database-fallback).
pub struct EntitlementResolver {
    tx_manager: TxManager,
    features: Arc<dyn FeatureRepository>,
    entitlements: Arc<dyn EntitlementRepository>,
    cache: Arc<dyn EntitlementCache>,
    clock: Arc<dyn Clock>,
    cache_ttl_secs: u64,
}

impl EntitlementResolver {
    pub fn new(
        tx_manager: TxManager,
        features: Arc<dyn FeatureRepository>,
        entitlements: Arc<dyn EntitlementRepository>,
        cache: Arc<dyn EntitlementCache>,
        clock: Arc<dyn Clock>,
        cache_ttl_secs: u64,
    ) -> Self {
        Self {
            tx_manager,
            features,
            entitlements,
            cache,
            clock,
            cache_ttl_secs,
        }
    }

    /// The resolved set for a tenant. Cache hits skip the database; on a
    /// miss the database is read and the result is cached. Backend failures
    /// fail closed — never fabricate an entitlement.
    pub async fn resolve(&self, tenant_id: Uuid) -> Result<ResolvedSet, AccessError> {
        if let Some(cached) = self.cache.get(tenant_id).await {
            return Ok(ResolvedSet::Cached(cached.entries));
        }
        let entries = self.resolve_from_db(tenant_id).await?;
        self.cache
            .put(tenant_id, &entries, self.cache_ttl_secs)
            .await;
        Ok(ResolvedSet::Fresh(entries))
    }

    async fn resolve_from_db(
        &self,
        tenant_id: Uuid,
    ) -> Result<Vec<ResolvedEntitlement>, AccessError> {
        let now = self.clock.now();

        // The catalog is global (no RLS) and its port is platform-scoped;
        // the entitlement rows are tenant-scoped (RLS). Two transactions,
        // each through the port that owns the table.
        let features = {
            let mut scope = self
                .tx_manager
                .begin_unscoped("entitlement_resolve_features", "access")
                .await
                .map_err(storage)?;
            let features = self.features.list_all(&mut scope).await?;
            scope.commit().await.map_err(storage)?;
            features
        };
        let rows = {
            let mut scope = self
                .tx_manager
                .begin_for(tenant_id)
                .await
                .map_err(storage)?;
            let rows = self
                .entitlements
                .list_for_tenant(&mut scope, tenant_id)
                .await?;
            scope.commit().await.map_err(storage)?;
            rows
        };

        Ok(resolve_all(&features, &rows, now))
    }

    /// Drop the cached set for a tenant (grant/revoke/expiry paths).
    pub async fn invalidate(&self, tenant_id: Uuid) {
        self.cache.invalidate(tenant_id).await;
    }
}

/// The [`FeatureGate`] implementation: resolution plus atomic quota
/// consumption. Injected into every context that has optional capability.
pub struct EntitlementFeatureGate {
    resolver: Arc<EntitlementResolver>,
    tx_manager: TxManager,
    quota: Arc<dyn QuotaUsageStore>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl EntitlementFeatureGate {
    pub fn new(
        resolver: Arc<EntitlementResolver>,
        tx_manager: TxManager,
        quota: Arc<dyn QuotaUsageStore>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            resolver,
            tx_manager,
            quota,
            events,
            clock,
        }
    }
}

#[async_trait::async_trait]
impl FeatureGate for EntitlementFeatureGate {
    async fn is_enabled(&self, tenant_id: Uuid, feature: &FeatureKey) -> Result<bool, AccessError> {
        let set = self.resolver.resolve(tenant_id).await?;
        let entry = set
            .find(feature)
            .ok_or_else(|| AccessError::FeatureNotFound {
                key: feature.to_string(),
            })?;
        Ok(entry.enabled)
    }

    async fn remaining(
        &self,
        tenant_id: Uuid,
        feature: &FeatureKey,
    ) -> Result<Option<i64>, AccessError> {
        let set = self.resolver.resolve(tenant_id).await?;
        let entry = set
            .find(feature)
            .ok_or_else(|| AccessError::FeatureNotFound {
                key: feature.to_string(),
            })?;
        match entry.kind {
            FeatureKind::Boolean => Ok(None),
            FeatureKind::Quota => {
                if !entry.enabled {
                    return Err(AccessError::FeatureNotFound {
                        key: feature.to_string(),
                    });
                }
                let Some(limit) = entry.limit else {
                    return Ok(None);
                };
                let period = period_start(self.clock.now());
                let mut scope = self
                    .tx_manager
                    .begin_for(tenant_id)
                    .await
                    .map_err(storage)?;
                let used = self
                    .quota
                    .usage(&mut scope, tenant_id, feature, period)
                    .await?;
                scope.commit().await.map_err(storage)?;
                Ok(Some(limit - used))
            }
        }
    }

    async fn consume(
        &self,
        tenant_id: Uuid,
        feature: &FeatureKey,
        amount: i64,
    ) -> Result<(), AccessError> {
        if amount < 0 {
            return Err(AccessError::InvalidLimit(format!(
                "cannot consume a negative amount: {amount}"
            )));
        }
        let set = self.resolver.resolve(tenant_id).await?;
        let entry = set
            .find(feature)
            .ok_or_else(|| AccessError::FeatureNotFound {
                key: feature.to_string(),
            })?;
        match entry.kind {
            FeatureKind::Boolean => {
                return Err(AccessError::InvalidEntitlement(format!(
                    "feature {feature} is boolean; it cannot be consumed"
                )));
            }
            FeatureKind::Quota => {
                if !entry.enabled {
                    return Err(AccessError::FeatureNotFound {
                        key: feature.to_string(),
                    });
                }
                // No limit configured: consumption is a no-op (recorded by
                // the caller's own business logic, not by the gate).
                let Some(limit) = entry.limit else {
                    return Ok(());
                };
                let period = period_start(self.clock.now());
                let mut scope = self
                    .tx_manager
                    .begin_for(tenant_id)
                    .await
                    .map_err(storage)?;
                match self
                    .quota
                    .consume(&mut scope, tenant_id, feature, period, amount, limit)
                    .await?
                {
                    Some(_used) => {
                        scope.commit().await.map_err(storage)?;
                        Ok(())
                    }
                    None => {
                        // Record the breach in the same transaction as the
                        // failed consumption, so the audit trail and the
                        // metric can tie it to the causal request.
                        let event = AccessEvent::QuotaExceeded {
                            tenant_id,
                            feature_key: feature.to_string(),
                            limit,
                            attempted: amount,
                        };
                        self.events.publish_scoped(&mut scope, &[event]).await?;
                        scope.commit().await.map_err(storage)?;
                        Err(AccessError::QuotaExceeded {
                            feature: feature.to_string(),
                            limit,
                            attempted: amount,
                        })
                    }
                }
            }
        }
    }
}
