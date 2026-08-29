//! The entitlement syncer: billing's side of the access context's
//! subscription-source rows. It writes through the access context's
//! `EntitlementRepository` port *and* appends `billing.entitlements_
//! recomputed` to the outbox in the same transaction — the worker consumes
//! that event to invalidate entitlement caches, so no caller can forget
//! either half.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_access::domain::{
    Entitlement, EntitlementCache, EntitlementRepository, EntitlementSource, FeatureKey,
};
use sanvi_kernel::{EventId, RawIdGenerator, TenantId};
use sanvi_messaging::{ConsumerHandler, OutboxWriter};
use sanvi_persistence::PlatformScope;

use crate::domain::events::BillingEvent;
use crate::domain::{BillingError, EntitlementGrant, EntitlementSyncer};

/// A minimal clock for row timestamps (updated_at on the rows we write).
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncerClock;

impl SyncerClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub struct AccessEntitlementSyncer {
    entitlements: std::sync::Arc<dyn EntitlementRepository>,
    ids: std::sync::Arc<dyn RawIdGenerator>,
}

impl AccessEntitlementSyncer {
    pub fn new(
        entitlements: std::sync::Arc<dyn EntitlementRepository>,
        ids: std::sync::Arc<dyn RawIdGenerator>,
    ) -> Self {
        Self { entitlements, ids }
    }

    fn next_event_id(&self) -> Result<EventId, BillingError> {
        self.ids
            .next_id()
            .map(EventId::from_snowflake)
            .map_err(|err| BillingError::Storage(err.to_string()))
    }

    /// Replace the tenant's subscription-source rows with `grants`.
    async fn write(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        plan_key: &str,
        grants: &[EntitlementGrant],
        clock: SyncerClock,
    ) -> Result<(), BillingError> {
        let now = clock.now();
        let current = self
            .entitlements
            .list_for_tenant_platform(scope, tenant_id)
            .await
            .map_err(|err| BillingError::Storage(err.to_string()))?;
        let current_keys: std::collections::BTreeSet<String> = current
            .iter()
            .filter(|row| row.source() == EntitlementSource::Subscription)
            .map(|row| row.feature().as_str().to_string())
            .collect();

        for grant in grants {
            let key = FeatureKey::parse(&grant.feature_key)
                .map_err(|err| BillingError::InvalidEntitlement(err.to_string()))?;
            let row = Entitlement::from_parts(
                tenant_id,
                key,
                grant.enabled,
                grant.limit,
                EntitlementSource::Subscription,
                None,
                None,
                Some(format!("plan: {plan_key}")),
                now,
            );
            self.entitlements
                .save(scope, &row)
                .await
                .map_err(|err| BillingError::Storage(err.to_string()))?;
        }

        // Drop subscription rows for features the plan no longer grants.
        for key in &current_keys {
            if !grants.iter().any(|grant| grant.feature_key == *key) {
                let feature = FeatureKey::parse(key)
                    .map_err(|err| BillingError::InvalidEntitlement(err.to_string()))?;
                self.entitlements
                    .delete(scope, tenant_id, &feature, EntitlementSource::Subscription)
                    .await
                    .map_err(|err| BillingError::Storage(err.to_string()))?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl EntitlementSyncer for AccessEntitlementSyncer {
    async fn sync_subscription(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        plan_key: &str,
        grants: &[EntitlementGrant],
    ) -> Result<(), BillingError> {
        self.write(scope, tenant_id, plan_key, grants, SyncerClock)
            .await?;
        OutboxWriter
            .append(
                scope.tx_mut(),
                &BillingEvent::EntitlementsRecomputed { tenant_id }.envelope(self.next_event_id()?),
            )
            .await
            .map_err(|err| BillingError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn clear_subscription(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<(), BillingError> {
        let current = self
            .entitlements
            .list_for_tenant_platform(scope, tenant_id)
            .await
            .map_err(|err| BillingError::Storage(err.to_string()))?;
        for row in current
            .iter()
            .filter(|row| row.source() == EntitlementSource::Subscription)
        {
            self.entitlements
                .delete(
                    scope,
                    tenant_id,
                    row.feature(),
                    EntitlementSource::Subscription,
                )
                .await
                .map_err(|err| BillingError::Storage(err.to_string()))?;
        }
        OutboxWriter
            .append(
                scope.tx_mut(),
                &BillingEvent::EntitlementsRecomputed { tenant_id }.envelope(self.next_event_id()?),
            )
            .await
            .map_err(|err| BillingError::Storage(err.to_string()))?;
        Ok(())
    }
}

/// Build the worker's billing consumer: on `billing.entitlements_recomputed`
/// it invalidates the affected tenant's entitlement cache, so the next
/// entitlement resolution reads the freshly written rows. Handlers are
/// idempotent by construction (dedup key in `platform.processed_events`).
pub fn billing_event_handler(cache: std::sync::Arc<dyn EntitlementCache>) -> ConsumerHandler {
    std::sync::Arc::new(move |envelope: sanvi_kernel::EventEnvelope| {
        let cache = cache.clone();
        Box::pin(async move {
            if envelope.event_type == "billing.entitlements_recomputed" {
                let tenant_id = envelope
                    .metadata
                    .get("tenant_id")
                    .and_then(|id| id.as_str())
                    .and_then(|id| id.parse::<TenantId>().ok())
                    .unwrap_or_else(|| TenantId::from_snowflake(envelope.aggregate_id));
                cache.invalidate(tenant_id).await;
            }
            Ok(())
        })
    })
}
