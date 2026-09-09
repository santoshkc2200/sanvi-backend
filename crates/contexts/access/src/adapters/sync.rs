use async_trait::async_trait;
use sanvi_kernel::{EventEnvelope, RoleId, TenantId, UserId};
use sanvi_messaging::ConsumerHandler;
use std::sync::Arc;

use crate::domain::AccessError;
use crate::domain::ports::{DecisionCache, EntitlementCache, PermissionStore, Tuple};

/// Store for tracking membership version to reject out-of-order and duplicate events.
#[async_trait]
pub trait MembershipVersionStore: Send + Sync {
    /// Checks whether the incoming version is newer than the recorded version for
    /// `(tenant_id, user_id)`. If newer or unversioned (`version <= 0`), records it and returns `Ok(true)`.
    /// If older or equal, returns `Ok(false)` (duplicate or out-of-order delivery).
    async fn check_and_update(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
        version: i64,
    ) -> Result<bool, AccessError>;
}

/// In-memory membership version store for single-process environments and tests.
#[derive(Default)]
pub struct InMemoryMembershipVersionStore {
    versions: std::sync::Mutex<std::collections::HashMap<(TenantId, UserId), i64>>,
}

impl InMemoryMembershipVersionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MembershipVersionStore for InMemoryMembershipVersionStore {
    async fn check_and_update(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
        version: i64,
    ) -> Result<bool, AccessError> {
        if version <= 0 {
            return Ok(true);
        }
        let mut map = self.versions.lock().expect("version store mutex poisoned");
        let current = map.entry((tenant_id, user_id)).or_insert(0);
        if version <= *current {
            return Ok(false);
        }
        *current = version;
        Ok(true)
    }
}

/// Redis-backed membership version store shared across workers.
#[derive(Clone)]
pub struct RedisMembershipVersionStore {
    conn: redis::aio::ConnectionManager,
}

impl RedisMembershipVersionStore {
    pub fn new(conn: redis::aio::ConnectionManager) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl MembershipVersionStore for RedisMembershipVersionStore {
    async fn check_and_update(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
        version: i64,
    ) -> Result<bool, AccessError> {
        if version <= 0 {
            return Ok(true);
        }
        let key = format!("sanvi:membership_ver:{tenant_id}:{user_id}");
        let mut conn = self.conn.clone();
        let script = redis::Script::new(
            r#"
            local cur = redis.call('GET', KEYS[1])
            if cur and tonumber(cur) >= tonumber(ARGV[1]) then
                return 0
            else
                redis.call('SET', KEYS[1], ARGV[1])
                return 1
            end
            "#,
        );
        let res: i64 = script
            .key(&key)
            .arg(version)
            .invoke_async(&mut conn)
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(res == 1)
    }
}

/// A consumer for the `sanvi.events.identity` stream.
///
/// Membership events carry everything needed to update tuples without a
/// database read: the tenant, user and role ids. Role *keys* must be
/// resolved, though — tuples name roles by key — so the handler takes a
/// resolver callback (the worker wires a DB-backed one).
pub fn access_sync_handler(
    store: Arc<dyn PermissionStore>,
    cache: Option<Arc<dyn DecisionCache>>,
    role_key_resolver: impl Fn(
        RoleId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<String>> + Send>,
    > + Send
    + Sync
    + 'static,
) -> ConsumerHandler {
    access_sync_handler_with_version_store(
        store,
        cache,
        Arc::new(InMemoryMembershipVersionStore::new()),
        role_key_resolver,
    )
}

pub fn access_sync_handler_with_version_store(
    store: Arc<dyn PermissionStore>,
    cache: Option<Arc<dyn DecisionCache>>,
    version_store: Arc<dyn MembershipVersionStore>,
    role_key_resolver: impl Fn(
        RoleId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<String>> + Send>,
    > + Send
    + Sync
    + 'static,
) -> ConsumerHandler {
    let role_key_resolver = Arc::new(role_key_resolver);
    Arc::new(move |envelope: EventEnvelope| {
        let store = store.clone();
        let cache = cache.clone();
        let version_store = version_store.clone();
        let role_key_resolver = role_key_resolver.clone();
        Box::pin(async move {
            match apply_event(&store, &version_store, envelope, &role_key_resolver).await {
                Ok((user_id, changed)) => {
                    if changed && let (Some(cache), Some(user_id)) = (&cache, user_id) {
                        cache.invalidate_user(user_id).await;
                    }
                    Ok(())
                }
                Err(err) => {
                    // Consumer-level retry + dead-letter policy handles
                    // poison messages; here we surface the failure so it can be retried.
                    tracing::error!(error = %err, "access sync failed");
                    Err(anyhow::anyhow!(err.to_string()))
                }
            }
        })
    })
}

async fn apply_event(
    store: &Arc<dyn PermissionStore>,
    version_store: &Arc<dyn MembershipVersionStore>,
    envelope: EventEnvelope,
    role_key_resolver: &Arc<
        impl Fn(RoleId) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync
        + 'static,
    >,
) -> Result<(Option<UserId>, bool), AccessError> {
    match envelope.event_type.as_str() {
        "identity.membership_granted" => {
            let (tenant_id, user_id, role_ids, _removed, version) =
                parse_membership_payload(&envelope)?;
            if !version_store
                .check_and_update(tenant_id, user_id, version)
                .await?
            {
                tracing::info!(tenant_id = %tenant_id, user_id = %user_id, version, "skipping stale or duplicate membership_granted event");
                return Ok((None, false));
            }
            let tuples =
                membership_tuples(tenant_id, user_id, &role_ids, role_key_resolver).await?;
            store.write(&tuples).await?;
            Ok((Some(user_id), true))
        }
        "identity.membership_roles_changed" => {
            let (tenant_id, user_id, role_ids, removed_role_ids, version) =
                parse_membership_payload(&envelope)?;
            if !version_store
                .check_and_update(tenant_id, user_id, version)
                .await?
            {
                tracing::info!(tenant_id = %tenant_id, user_id = %user_id, version, "skipping stale or duplicate membership_roles_changed event");
                return Ok((None, false));
            }
            // First delete removed roles if any
            let removed_tuples =
                resolve_role_tuples(tenant_id, user_id, &removed_role_ids, role_key_resolver)
                    .await?;
            if !removed_tuples.is_empty() {
                store.delete(&removed_tuples).await?;
            }
            // Then write current/new roles and tenant membership
            let tuples =
                membership_tuples(tenant_id, user_id, &role_ids, role_key_resolver).await?;
            store.write(&tuples).await?;
            Ok((Some(user_id), true))
        }
        "identity.membership_revoked" => {
            let (tenant_id, user_id, role_ids, _removed, version) =
                parse_membership_payload(&envelope)?;
            if !version_store
                .check_and_update(tenant_id, user_id, version)
                .await?
            {
                tracing::info!(tenant_id = %tenant_id, user_id = %user_id, version, "skipping stale or duplicate membership_revoked event");
                return Ok((None, false));
            }
            let mut tuples = vec![Tuple::tenant_membership(tenant_id, user_id)];
            let role_tuples =
                resolve_role_tuples(tenant_id, user_id, &role_ids, role_key_resolver).await?;
            tuples.extend(role_tuples);
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
            Ok((Some(user_id), true))
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
) -> Result<(TenantId, UserId, Vec<RoleId>, Vec<RoleId>, i64), AccessError> {
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
    let removed_role_ids = envelope
        .payload
        .get("removed_role_ids")
        .and_then(|value| value.as_array())
        .map(|roles| {
            roles
                .iter()
                .filter_map(|role| role.as_str())
                .filter_map(|role| role.parse::<RoleId>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let version = envelope
        .payload
        .get("version")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    Ok((tenant_id, user_id, role_ids, removed_role_ids, version))
}

async fn resolve_role_tuples(
    tenant_id: TenantId,
    user_id: UserId,
    role_ids: &[RoleId],
    role_key_resolver: &Arc<
        impl Fn(RoleId) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync
        + 'static,
    >,
) -> Result<Vec<Tuple>, AccessError> {
    let mut tuples = Vec::with_capacity(role_ids.len());
    for role_id in role_ids {
        let key = (role_key_resolver)(*role_id)
            .await
            .ok_or(AccessError::RoleKeyResolutionFailed(*role_id))?;
        tuples.push(Tuple::role_membership(
            &format!("{tenant_id}:{key}"),
            user_id,
        ));
    }
    Ok(tuples)
}

async fn membership_tuples(
    tenant_id: TenantId,
    user_id: UserId,
    role_ids: &[RoleId],
    role_key_resolver: &Arc<
        impl Fn(RoleId) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync
        + 'static,
    >,
) -> Result<Vec<Tuple>, AccessError> {
    let mut tuples = vec![Tuple::tenant_membership(tenant_id, user_id)];
    let role_tuples = resolve_role_tuples(tenant_id, user_id, role_ids, role_key_resolver).await?;
    tuples.extend(role_tuples);
    Ok(tuples)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockPermissionStore {
        written: Mutex<Vec<Tuple>>,
        deleted: Mutex<Vec<Tuple>>,
    }

    #[async_trait]
    impl PermissionStore for MockPermissionStore {
        async fn write(&self, tuples: &[Tuple]) -> Result<(), AccessError> {
            self.written.lock().unwrap().extend(tuples.to_vec());
            Ok(())
        }
        async fn delete(&self, tuples: &[Tuple]) -> Result<(), AccessError> {
            self.deleted.lock().unwrap().extend(tuples.to_vec());
            Ok(())
        }
        async fn clear_namespace(&self, _namespace: &str) -> Result<(), AccessError> {
            Ok(())
        }
        async fn check_membership(
            &self,
            _user: UserId,
            _objs: &[String],
        ) -> Result<Vec<bool>, AccessError> {
            Ok(vec![])
        }
    }

    #[derive(Default)]
    struct MockDecisionCache {
        invalidated_users: Mutex<Vec<UserId>>,
    }

    #[async_trait]
    impl DecisionCache for MockDecisionCache {
        async fn get(&self, _key: &str) -> Option<crate::domain::ports::Decision> {
            None
        }
        async fn put(&self, _key: &str, _decision: crate::domain::ports::Decision, _ttl_secs: u64) {
        }
        async fn invalidate_user(&self, user_id: UserId) {
            self.invalidated_users.lock().unwrap().push(user_id);
        }
        async fn invalidate_all(&self) {}
    }

    fn sample_envelope(event_type: &str, payload: serde_json::Value) -> EventEnvelope {
        EventEnvelope::new(
            sanvi_kernel::EventId::try_from(1_i64).unwrap(),
            "identity.membership",
            sanvi_kernel::SnowflakeId::try_from(1_i64).unwrap(),
            event_type,
            payload,
        )
    }

    #[tokio::test]
    async fn roles_changed_reconciles_and_deletes_removed_tuples() {
        let store = Arc::new(MockPermissionStore::default());
        let cache = Arc::new(MockDecisionCache::default());
        let version_store = Arc::new(InMemoryMembershipVersionStore::new());

        let tenant_id = TenantId::try_from(10).unwrap();
        let user_id = UserId::try_from(100).unwrap();
        let role_old = RoleId::try_from(1).unwrap();
        let role_new = RoleId::try_from(2).unwrap();

        let handler = access_sync_handler_with_version_store(
            store.clone(),
            Some(cache.clone()),
            version_store,
            move |role_id| {
                Box::pin(async move {
                    if role_id == role_old {
                        Some("old_role".to_string())
                    } else if role_id == role_new {
                        Some("new_role".to_string())
                    } else {
                        None
                    }
                })
            },
        );

        let event = sample_envelope(
            "identity.membership_roles_changed",
            serde_json::json!({
                "tenant_id": tenant_id.to_string(),
                "user_id": user_id.to_string(),
                "role_ids": [role_new.to_string()],
                "removed_role_ids": [role_old.to_string()],
                "version": 100,
            }),
        );

        handler(event).await.expect("handler should succeed");

        let deleted = store.deleted.lock().unwrap().clone();
        assert_eq!(
            deleted,
            vec![Tuple::role_membership("10:old_role", user_id)]
        );

        let written = store.written.lock().unwrap().clone();
        assert_eq!(
            written,
            vec![
                Tuple::tenant_membership(tenant_id, user_id),
                Tuple::role_membership("10:new_role", user_id),
            ]
        );

        let invalidated = cache.invalidated_users.lock().unwrap().clone();
        assert_eq!(invalidated, vec![user_id]);
    }

    #[tokio::test]
    async fn role_resolution_failure_returns_error() {
        let store = Arc::new(MockPermissionStore::default());
        let version_store = Arc::new(InMemoryMembershipVersionStore::new());
        let tenant_id = TenantId::try_from(10).unwrap();
        let user_id = UserId::try_from(100).unwrap();
        let unknown_role = RoleId::try_from(999).unwrap();

        let handler =
            access_sync_handler_with_version_store(store.clone(), None, version_store, |_| {
                Box::pin(async { None })
            });

        let event = sample_envelope(
            "identity.membership_granted",
            serde_json::json!({
                "tenant_id": tenant_id.to_string(),
                "user_id": user_id.to_string(),
                "role_ids": [unknown_role.to_string()],
                "version": 1,
            }),
        );

        let result = handler(event).await;
        assert!(
            result.is_err(),
            "resolution miss must return error for retry"
        );
    }

    #[tokio::test]
    async fn unresolvable_role_key_is_an_error_not_a_silent_skip() {
        // A resolver miss used to drop the tuple and report success, leaving
        // the member without the role in Keto and nothing to retry. It must
        // surface so the consumer's retry/dead-letter policy sees it.
        let store = Arc::new(MockPermissionStore::default());
        let version_store = Arc::new(InMemoryMembershipVersionStore::new());
        let handler =
            access_sync_handler_with_version_store(store.clone(), None, version_store, |_| {
                Box::pin(async { None })
            });

        let event = sample_envelope(
            "identity.membership_granted",
            serde_json::json!({
                "tenant_id": TenantId::try_from(10).unwrap().to_string(),
                "user_id": UserId::try_from(100).unwrap().to_string(),
                "role_ids": [RoleId::try_from(1).unwrap().to_string()],
                "version": 1,
            }),
        );
        assert!(
            handler(event).await.is_err(),
            "an unresolved role key must fail the handler"
        );
        assert!(
            store.written.lock().unwrap().is_empty(),
            "no tuples may be written when a role key could not be resolved"
        );
    }

    #[tokio::test]
    async fn revocation_arriving_before_a_later_regrant_does_not_undo_it() {
        // Revoke (v1) and re-grant (v2) can be delivered out of order. The
        // re-grant must win regardless of arrival order.
        let store = Arc::new(MockPermissionStore::default());
        let version_store = Arc::new(InMemoryMembershipVersionStore::new());
        let tenant_id = TenantId::try_from(10).unwrap();
        let user_id = UserId::try_from(100).unwrap();
        let role_id = RoleId::try_from(1).unwrap();
        let handler =
            access_sync_handler_with_version_store(store.clone(), None, version_store, |_| {
                Box::pin(async { Some("admin".to_string()) })
            });

        let payload = |version: i64| {
            serde_json::json!({
                "tenant_id": tenant_id.to_string(),
                "user_id": user_id.to_string(),
                "role_ids": [role_id.to_string()],
                "version": version,
            })
        };

        handler(sample_envelope("identity.membership_granted", payload(2)))
            .await
            .expect("re-grant applies");
        handler(sample_envelope("identity.membership_revoked", payload(1)))
            .await
            .expect("stale revoke is skipped, not failed");

        assert!(
            store.deleted.lock().unwrap().is_empty(),
            "a revoke older than the applied state must not delete tuples"
        );
    }

    #[tokio::test]
    async fn duplicate_and_older_versions_are_discarded() {
        let store = Arc::new(MockPermissionStore::default());
        let version_store = Arc::new(InMemoryMembershipVersionStore::new());
        let tenant_id = TenantId::try_from(10).unwrap();
        let user_id = UserId::try_from(100).unwrap();
        let role_id = RoleId::try_from(1).unwrap();

        let handler =
            access_sync_handler_with_version_store(store.clone(), None, version_store, |_| {
                Box::pin(async { Some("admin".to_string()) })
            });

        // Version 200 arrives first
        let event_v200 = sample_envelope(
            "identity.membership_granted",
            serde_json::json!({
                "tenant_id": tenant_id.to_string(),
                "user_id": user_id.to_string(),
                "role_ids": [role_id.to_string()],
                "version": 200,
            }),
        );
        handler(event_v200).await.expect("v200 should succeed");
        assert_eq!(store.written.lock().unwrap().len(), 2);

        // Clear store written to check if subsequent events write anything
        store.written.lock().unwrap().clear();

        // Older version 100 arrives second (out-of-order delivery)
        let event_v100 = sample_envelope(
            "identity.membership_granted",
            serde_json::json!({
                "tenant_id": tenant_id.to_string(),
                "user_id": user_id.to_string(),
                "role_ids": [role_id.to_string()],
                "version": 100,
            }),
        );
        handler(event_v100)
            .await
            .expect("v100 should be handled (skipped)");
        assert!(
            store.written.lock().unwrap().is_empty(),
            "older version must not write tuples"
        );

        // Duplicate version 200 arrives
        let event_v200_dup = sample_envelope(
            "identity.membership_granted",
            serde_json::json!({
                "tenant_id": tenant_id.to_string(),
                "user_id": user_id.to_string(),
                "role_ids": [role_id.to_string()],
                "version": 200,
            }),
        );
        handler(event_v200_dup)
            .await
            .expect("dup should be handled (skipped)");
        assert!(
            store.written.lock().unwrap().is_empty(),
            "duplicate version must not write tuples"
        );
    }
}
