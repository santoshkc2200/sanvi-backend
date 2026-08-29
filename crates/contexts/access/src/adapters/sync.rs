//! The event → Keto sync: consumes identity events and applies them as
//! relation tuples, so a membership change reaches Keto within one outbox
//! poll without any process restart.

use sanvi_kernel::{EventEnvelope, RoleId, TenantId, UserId};
use sanvi_messaging::ConsumerHandler;

use crate::domain::AccessError;
use crate::domain::ports::{DecisionCache, EntitlementCache, PermissionStore, Tuple};

/// A consumer for the `sanvi.events.identity` stream.
///
/// Membership events carry everything needed to update tuples without a
/// database read: the tenant, user and role ids. Role *keys* must be
/// resolved, though — tuples name roles by key — so the handler takes a
/// resolver callback (the worker wires a DB-backed one).
pub fn access_sync_handler(
    store: std::sync::Arc<dyn PermissionStore>,
    cache: Option<std::sync::Arc<dyn DecisionCache>>,
    role_key_resolver: impl Fn(
        RoleId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<String>> + Send>,
    > + Send
    + Sync
    + 'static,
) -> ConsumerHandler {
    let role_key_resolver = std::sync::Arc::new(role_key_resolver);
    std::sync::Arc::new(move |envelope: EventEnvelope| {
        let store = store.clone();
        let cache = cache.clone();
        let role_key_resolver = role_key_resolver.clone();
        Box::pin(async move {
            match apply_event(&store, envelope, &role_key_resolver).await {
                Ok((user_id, changed)) => {
                    if changed && let (Some(cache), Some(user_id)) = (&cache, user_id) {
                        cache.invalidate_user(user_id).await;
                    }
                    Ok(())
                }
                Err(err) => {
                    // Consumer-level retry + dead-letter policy handles
                    // poison messages; here we just surface the failure.
                    tracing::error!(error = %err, "access sync failed");
                    Err(anyhow::anyhow!(err.to_string()))
                }
            }
        })
    })
}

async fn apply_event(
    store: &std::sync::Arc<dyn PermissionStore>,
    envelope: EventEnvelope,
    role_key_resolver: &std::sync::Arc<
        impl Fn(RoleId) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync
        + 'static,
    >,
) -> Result<(Option<UserId>, bool), AccessError> {
    match envelope.event_type.as_str() {
        "identity.membership_granted" | "identity.membership_roles_changed" => {
            let (tenant_id, user_id, role_ids) = parse_membership_payload(&envelope)?;
            let tuples = membership_tuples(tenant_id, user_id, &role_ids, role_key_resolver).await;
            store.write(&tuples).await?;
            Ok((Some(user_id), true))
        }
        "identity.membership_revoked" => {
            let (tenant_id, user_id, role_ids) = parse_membership_payload(&envelope)?;
            let mut tuples = vec![Tuple::tenant_membership(tenant_id, user_id)];
            for role_id in &role_ids {
                if let Some(key) = (role_key_resolver)(*role_id).await {
                    tuples.push(Tuple::role_membership(
                        &format!("{tenant_id}:{key}"),
                        user_id,
                    ));
                }
            }
            store.delete(&tuples).await?;
            Ok((Some(user_id), true))
        }
        "identity.user_deactivated" => {
            let user_id = envelope
                .payload
                .get("user_id")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<UserId>().ok())
                .unwrap_or_else(|| UserId::from_snowflake(envelope.aggregate_id));
            // Cache invalidation only; tuple removal is the rebuild's job.
            Ok((Some(user_id), false))
        }
        other => {
            tracing::debug!(
                event_type = other,
                "access sync: not an access-relevant event"
            );
            Ok((None, false))
        }
    }
}

fn parse_membership_payload(
    envelope: &EventEnvelope,
) -> Result<(TenantId, UserId, Vec<RoleId>), AccessError> {
    let tenant_id = envelope
        .metadata
        .get("tenant_id")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<TenantId>().ok())
        .or_else(|| {
            envelope
                .payload
                .get("tenant_id")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<TenantId>().ok())
        })
        .ok_or_else(|| AccessError::BackendRejected("membership event has no tenant_id".into()))?;
    let user_id = envelope
        .payload
        .get("user_id")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<UserId>().ok())
        .unwrap_or_else(|| UserId::from_snowflake(envelope.aggregate_id));
    let role_ids = envelope
        .payload
        .get("role_ids")
        .and_then(|value| value.as_array())
        .map(|roles| {
            roles
                .iter()
                .filter_map(|role| role.as_str())
                .filter_map(|role| role.parse::<RoleId>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok((tenant_id, user_id, role_ids))
}

async fn membership_tuples(
    tenant_id: TenantId,
    user_id: UserId,
    role_ids: &[RoleId],
    role_key_resolver: &std::sync::Arc<
        impl Fn(RoleId) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync
        + 'static,
    >,
) -> Vec<Tuple> {
    let mut tuples = vec![Tuple::tenant_membership(tenant_id, user_id)];
    for role_id in role_ids {
        if let Some(key) = (role_key_resolver)(*role_id).await {
            tuples.push(Tuple::role_membership(
                &format!("{tenant_id}:{key}"),
                user_id,
            ));
        }
    }
    tuples
}

// ---------------------------------------------------------------------------
// The access-event consumer (sanvi.events.access): entitlement cache
// invalidation, feature-catalog cache invalidation, and Keto sync for role
// authoring.
// ---------------------------------------------------------------------------

/// Build the consumer for the `sanvi.events.access` stream. Three
/// responsibilities:
///
/// 1. Entitlement events → drop the affected tenant's cached resolution.
/// 2. Feature definitions → drop every cached resolution (defaults may have
///    changed for tenants without their own rows).
/// 3. Role authoring events → keep Keto's `permission#granted` tuples in
///    sync with the new/updated/deleted role, and invalidate cached
///    authorization decisions (a role change affects everyone holding it).
pub fn entitlement_sync_handler(
    cache: Option<std::sync::Arc<dyn EntitlementCache>>,
    decision_cache: Option<std::sync::Arc<dyn DecisionCache>>,
    store: Option<std::sync::Arc<dyn PermissionStore>>,
) -> ConsumerHandler {
    std::sync::Arc::new(move |envelope: EventEnvelope| {
        let cache = cache.clone();
        let decision_cache = decision_cache.clone();
        let store = store.clone();
        Box::pin(async move {
            if let Err(err) = apply_access_event(&envelope, &cache, &decision_cache, &store).await {
                tracing::error!(error = %err, event_type = %envelope.event_type, "access sync failed");
                return Err(anyhow::anyhow!(err.to_string()));
            }
            Ok(())
        })
    })
}

async fn apply_access_event(
    envelope: &EventEnvelope,
    cache: &Option<std::sync::Arc<dyn EntitlementCache>>,
    decision_cache: &Option<std::sync::Arc<dyn DecisionCache>>,
    store: &Option<std::sync::Arc<dyn PermissionStore>>,
) -> Result<(), AccessError> {
    let tenant_id = envelope
        .metadata
        .get("tenant_id")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<TenantId>().ok());

    match envelope.event_type.as_str() {
        "access.entitlement_granted"
        | "access.entitlement_revoked"
        | "access.entitlement_expired" => {
            if let (Some(cache), Some(tenant_id)) = (cache, tenant_id) {
                cache.invalidate(tenant_id).await;
            }
        }
        "access.feature_defined" => {
            if let Some(cache) = cache {
                cache.invalidate_all().await;
            }
        }
        "access.quota_exceeded" => {
            // Already recorded in the database (the failed consumption
            // committed the event); the metric comes from the Prometheus
            // counters the gate increments. Nothing to sync.
            tracing::warn!(?tenant_id, "quota exceeded event observed");
        }
        "access.role_created" => {
            let role_object = role_object(envelope);
            let permissions = string_list(&envelope.payload, "permissions");
            if let Some(store) = store {
                let tuples: Vec<Tuple> = permissions
                    .iter()
                    .map(|permission| Tuple::permission_grant(permission, &role_object))
                    .collect();
                store.write(&tuples).await?;
            }
            if let Some(cache) = decision_cache {
                cache.invalidate_all().await;
            }
        }
        "access.role_updated" => {
            let role_object = role_object(envelope);
            let added = string_list(&envelope.payload, "added_permissions");
            let removed = string_list(&envelope.payload, "removed_permissions");
            if let Some(store) = store {
                let to_write: Vec<Tuple> = added
                    .iter()
                    .map(|permission| Tuple::permission_grant(permission, &role_object))
                    .collect();
                if !to_write.is_empty() {
                    store.write(&to_write).await?;
                }
                let to_delete: Vec<Tuple> = removed
                    .iter()
                    .map(|permission| Tuple::permission_grant(permission, &role_object))
                    .collect();
                if !to_delete.is_empty() {
                    store.delete(&to_delete).await?;
                }
            }
            if let Some(cache) = decision_cache {
                cache.invalidate_all().await;
            }
        }
        "access.role_deleted" => {
            let role_object = role_object(envelope);
            let permissions = string_list(&envelope.payload, "permissions");
            if let Some(store) = store {
                let tuples: Vec<Tuple> = permissions
                    .iter()
                    .map(|permission| Tuple::permission_grant(permission, &role_object))
                    .collect();
                store.delete(&tuples).await?;
            }
            if let Some(cache) = decision_cache {
                cache.invalidate_all().await;
            }
        }
        other => {
            tracing::debug!(
                event_type = other,
                "access sync: not an access-relevant event"
            );
        }
    }
    Ok(())
}

/// The Keto role object for a role event: `platform:{key}` for platform
/// roles, `{tenant_id}:{key}` for tenant roles.
fn role_object(envelope: &EventEnvelope) -> String {
    let key = envelope
        .payload
        .get("key")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let tenant_id = envelope
        .payload
        .get("tenant_id")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<TenantId>().ok());
    match tenant_id {
        Some(tenant_id) => format!("{tenant_id}:{key}"),
        None => format!("platform:{key}"),
    }
}

fn string_list(payload: &serde_json::Value, field: &str) -> Vec<String> {
    payload
        .get(field)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
