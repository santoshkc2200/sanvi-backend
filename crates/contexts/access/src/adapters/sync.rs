//! The event → Keto sync: consumes identity events and applies them as
//! relation tuples, so a membership change reaches Keto within one outbox
//! poll without any process restart.

use sanvi_kernel::EventEnvelope;
use sanvi_messaging::ConsumerHandler;

use crate::domain::AccessError;
use crate::domain::ports::{DecisionCache, PermissionStore, Tuple};

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
        uuid::Uuid,
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
        impl Fn(
            uuid::Uuid,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync
        + 'static,
    >,
) -> Result<(Option<uuid::Uuid>, bool), AccessError> {
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
                .and_then(|value| value.parse::<uuid::Uuid>().ok())
                .unwrap_or(envelope.aggregate_id);
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
) -> Result<(uuid::Uuid, uuid::Uuid, Vec<uuid::Uuid>), AccessError> {
    let tenant_id = envelope
        .metadata
        .get("tenant_id")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<uuid::Uuid>().ok())
        .or_else(|| {
            envelope
                .payload
                .get("tenant_id")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<uuid::Uuid>().ok())
        })
        .ok_or_else(|| AccessError::BackendRejected("membership event has no tenant_id".into()))?;
    let user_id = envelope
        .payload
        .get("user_id")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<uuid::Uuid>().ok())
        .unwrap_or(envelope.aggregate_id);
    if user_id.is_nil() {
        return Err(AccessError::BackendRejected(
            "membership event has no user_id".into(),
        ));
    }
    let role_ids = envelope
        .payload
        .get("role_ids")
        .and_then(|value| value.as_array())
        .map(|roles| {
            roles
                .iter()
                .filter_map(|role| role.as_str())
                .filter_map(|role| role.parse::<uuid::Uuid>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok((tenant_id, user_id, role_ids))
}

async fn membership_tuples(
    tenant_id: uuid::Uuid,
    user_id: uuid::Uuid,
    role_ids: &[uuid::Uuid],
    role_key_resolver: &std::sync::Arc<
        impl Fn(
            uuid::Uuid,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
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
