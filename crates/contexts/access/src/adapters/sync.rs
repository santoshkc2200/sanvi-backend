//! Events wake serialized snapshot reconciliation; stale deltas never decide
//! the target authorization state. Failed projections remain retryable.
use crate::domain::{AccessError, AccessProjection, DecisionCache, EntitlementCache, Retryability};
use sanvi_kernel::{EventEnvelope, RoleId, TenantId, UserId};
use sanvi_messaging::ConsumerHandler;
use std::sync::Arc;

fn handler_error(err: AccessError) -> anyhow::Error {
    match err.retryability() {
        Retryability::Transient => sanvi_messaging::HandlerError::transient(err).into(),
        Retryability::Permanent => sanvi_messaging::HandlerError::permanent(err).into(),
    }
}

pub fn access_sync_handler(
    projection: Arc<dyn AccessProjection>,
    cache: Option<Arc<dyn DecisionCache>>,
) -> ConsumerHandler {
    Arc::new(move |envelope: EventEnvelope| {
        let projection = projection.clone();
        let cache = cache.clone();
        Box::pin(async move {
            match envelope.event_type.as_str() {
                "identity.membership_granted"
                | "identity.membership_roles_changed"
                | "identity.membership_revoked" => {
                    let tenant =
                        payload_id::<TenantId>(&envelope, "tenant_id").map_err(handler_error)?;
                    let user = payload_id::<UserId>(&envelope, "user_id").map_err(handler_error)?;
                    projection
                        .reconcile_membership(tenant, user)
                        .await
                        .map_err(handler_error)?;
                    if let Some(cache) = cache {
                        cache.invalidate_user(user).await;
                    }
                }
                "identity.user_deactivated" => {
                    let user = envelope
                        .payload
                        .get("user_id")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|value| value.parse::<UserId>().ok())
                        .unwrap_or_else(|| UserId::from_snowflake(envelope.aggregate_id));
                    if let Some(cache) = cache {
                        cache.invalidate_user(user).await;
                    }
                }
                _ => {}
            }
            Ok(())
        })
    })
}

fn payload_id<T: std::str::FromStr>(envelope: &EventEnvelope, key: &str) -> Result<T, AccessError> {
    envelope
        .payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            AccessError::Storage(format!("missing or invalid {key} in authorization event"))
        })
}

/// Entitlement/feature invalidation and authoritative role reconciliation.
pub fn entitlement_sync_handler(
    cache: Option<Arc<dyn EntitlementCache>>,
    decision_cache: Option<Arc<dyn DecisionCache>>,
    projection: Option<Arc<dyn AccessProjection>>,
) -> ConsumerHandler {
    Arc::new(move |envelope: EventEnvelope| {
        let cache = cache.clone();
        let decision_cache = decision_cache.clone();
        let projection = projection.clone();
        Box::pin(async move {
            apply_access_event(&envelope, &cache, &decision_cache, &projection)
                .await
                .map_err(handler_error)
        })
    })
}

async fn apply_access_event(
    envelope: &EventEnvelope,
    cache: &Option<Arc<dyn EntitlementCache>>,
    decision_cache: &Option<Arc<dyn DecisionCache>>,
    projection: &Option<Arc<dyn AccessProjection>>,
) -> Result<(), AccessError> {
    let tenant = envelope
        .metadata
        .get("tenant_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.parse::<TenantId>().ok());
    match envelope.event_type.as_str() {
        "access.entitlement_granted"
        | "access.entitlement_revoked"
        | "access.entitlement_expired" => {
            if let (Some(cache), Some(tenant)) = (cache, tenant) {
                cache.invalidate(tenant).await;
            }
        }
        "access.feature_defined" => {
            if let Some(cache) = cache {
                cache.invalidate_all().await;
            }
        }
        "access.quota_exceeded" => tracing::warn!(?tenant, "quota exceeded event observed"),
        "access.role_created" | "access.role_updated" | "access.role_deleted" => {
            let role = payload_id::<RoleId>(envelope, "role_id")?;
            let key = envelope
                .payload
                .get("key")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    AccessError::Storage("missing role key in authorization event".into())
                })?;
            let previous_object = match envelope
                .payload
                .get("tenant_id")
                .and_then(serde_json::Value::as_str)
            {
                Some(tenant) => format!("{tenant}:{key}"),
                None => format!("platform:{key}"),
            };
            let projection = projection
                .as_ref()
                .ok_or_else(|| AccessError::Storage("role projection is not configured".into()))?;
            projection.reconcile_role(role, &previous_object).await?;
            if let Some(cache) = decision_cache {
                cache.invalidate_all().await;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    struct UnavailableProjection;
    #[async_trait]
    impl AccessProjection for UnavailableProjection {
        async fn reconcile_membership(&self, _: TenantId, _: UserId) -> Result<(), AccessError> {
            Err(AccessError::BackendUnreachable("temporary outage".into()))
        }
        async fn reconcile_role(&self, _: RoleId, _: &str) -> Result<(), AccessError> {
            Err(AccessError::BackendUnreachable("temporary outage".into()))
        }
    }
    #[tokio::test]
    async fn projection_failure_preserves_transient_retry_classification() {
        let handler = access_sync_handler(Arc::new(UnavailableProjection), None);
        let event = EventEnvelope::new(
            sanvi_kernel::EventId::try_from(1).unwrap(),
            "identity.membership",
            sanvi_kernel::SnowflakeId::try_from(1).unwrap(),
            "identity.membership_revoked",
            serde_json::json!({"tenant_id":"10", "user_id":"100"}),
        );
        let error = handler(event).await.unwrap_err();
        assert!(sanvi_messaging::is_transient_error(&error));
    }
}
