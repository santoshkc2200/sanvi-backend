//! Entitlement use cases: operator overrides (grant/revoke with four-eyes
//! on large limits), the tenant's resolved view, and the expiry sweep.
//!
//! Every mutation publishes an `access.entitlement_*` event and an audit
//! entry in the same transaction as the state change; the cache is
//! invalidated after commit so readers never see the pre-change value.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::{SnowflakeId, TenantId, UserId};
use sanvi_persistence::TxManager;

use crate::application::feature_gate::EntitlementResolver;
use crate::application::storage;
use crate::domain::events::AccessEvent;
use crate::domain::ports::{ApprovalGate, ApprovalOutcome, EntitlementCache, EventPublisher};
use crate::domain::{
    AccessError, Clock, Entitlement, EntitlementRepository, EntitlementSource, FeatureKey,
    FeatureRepository, ResolvedEntitlement,
};

/// A four-eyes policy: overrides whose limit exceeds the threshold must be
/// approved by a second platform admin before they apply.
#[derive(Debug, Clone, Copy, Default)]
pub struct EntitlementPolicy {
    /// Overrides with a limit above this require approval (`None` disables
    /// four-eyes for overrides).
    pub approval_threshold: Option<i64>,
}

pub struct GrantEntitlementCommand {
    pub tenant_id: TenantId,
    pub feature_key: FeatureKey,
    pub enabled: bool,
    pub limit: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
    /// Internal: set by the four-eyes executor once the approval is given,
    /// so the re-run does not request approval again.
    pub skip_approval: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantEntitlementOutput {
    Granted,
    ApprovalRequired { approval_id: SnowflakeId },
}

pub struct GrantEntitlement {
    tx_manager: TxManager,
    features: Arc<dyn FeatureRepository>,
    entitlements: Arc<dyn EntitlementRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
    approval: Arc<dyn ApprovalGate>,
    policy: EntitlementPolicy,
}

impl GrantEntitlement {
    pub fn new(
        tx_manager: TxManager,
        features: Arc<dyn FeatureRepository>,
        entitlements: Arc<dyn EntitlementRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
        approval: Arc<dyn ApprovalGate>,
        policy: EntitlementPolicy,
    ) -> Self {
        Self {
            tx_manager,
            features,
            entitlements,
            events,
            clock,
            audit,
            approval,
            policy,
        }
    }

    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        cmd: GrantEntitlementCommand,
    ) -> Result<GrantEntitlementOutput, AccessError> {
        let now = self.clock.now();
        let mut scope = self
            .tx_manager
            .begin_unscoped("grant_entitlement", "platform-admin")
            .await
            .map_err(storage)?;

        let feature = self
            .features
            .find(&mut scope, &cmd.feature_key)
            .await?
            .ok_or_else(|| AccessError::FeatureNotFound {
                key: cmd.feature_key.to_string(),
            })?;
        if feature.kind() == crate::domain::FeatureKind::Boolean && cmd.limit.is_some() {
            return Err(AccessError::InvalidLimit(format!(
                "feature {} is boolean; it cannot carry a limit",
                cmd.feature_key
            )));
        }

        // Four-eyes: large overrides become pending approvals. The approval
        // executor re-runs this command with `skip_approval`.
        if !cmd.skip_approval
            && let Some(threshold) = self.policy.approval_threshold
            && let Some(limit) = cmd.limit
            && limit > threshold
        {
            let payload = serde_json::json!({
                "tenant_id": cmd.tenant_id,
                "feature_key": cmd.feature_key.as_str(),
                "enabled": cmd.enabled,
                "limit": cmd.limit,
                "expires_at": cmd.expires_at.map(|t| t.to_rfc3339()),
                "reason": cmd.reason,
            });
            let outcome = self
                .approval
                .request_if_required(&mut scope, actor, "entitlement.grant", payload)
                .await?;
            match outcome {
                ApprovalOutcome::Required { approval_id } => {
                    self.audit
                        .append(
                            &mut scope,
                            &AuditEntryData {
                                occurred_at: now,
                                actor_type: ActorType::PlatformAdmin,
                                actor_id: Some(actor),
                                on_behalf_of,
                                tenant_id: Some(cmd.tenant_id),
                                action: "access.entitlement_approval_requested".to_string(),
                                resource_type: Some("entitlement".to_string()),
                                resource_id: Some(cmd.feature_key.to_string()),
                                before: None,
                                after: Some(serde_json::json!({
                                    "approval_id": approval_id,
                                    "limit": cmd.limit,
                                })),
                                request_id: None,
                                ip: None,
                                user_agent: None,
                            },
                        )
                        .await
                        .map_err(|err| AccessError::Storage(err.to_string()))?;
                    scope.commit().await.map_err(storage)?;
                    return Ok(GrantEntitlementOutput::ApprovalRequired { approval_id });
                }
                ApprovalOutcome::NotRequired => {}
            }
        }

        let entitlement = Entitlement::new(
            cmd.tenant_id,
            cmd.feature_key.clone(),
            cmd.enabled,
            cmd.limit,
            EntitlementSource::Override,
            cmd.expires_at,
            Some(actor),
            cmd.reason.clone(),
            now,
        )?;
        self.entitlements.save(&mut scope, &entitlement).await?;
        self.events
            .publish(
                &mut scope,
                &[AccessEvent::EntitlementGranted {
                    tenant_id: cmd.tenant_id,
                    feature_key: cmd.feature_key.to_string(),
                    enabled: cmd.enabled,
                    limit: cmd.limit,
                    expires_at: cmd.expires_at,
                }],
            )
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: now,
                    actor_type: ActorType::PlatformAdmin,
                    actor_id: Some(actor),
                    on_behalf_of,
                    tenant_id: Some(cmd.tenant_id),
                    action: "access.entitlement_granted".to_string(),
                    resource_type: Some("entitlement".to_string()),
                    resource_id: Some(cmd.feature_key.to_string()),
                    before: None,
                    after: Some(serde_json::json!({
                        "enabled": cmd.enabled,
                        "limit": cmd.limit,
                        "source": "override",
                        "expires_at": cmd.expires_at.map(|t| t.to_rfc3339()),
                        "reason": cmd.reason,
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;

        Ok(GrantEntitlementOutput::Granted)
    }
}

pub struct RevokeEntitlementCommand {
    pub tenant_id: TenantId,
    pub feature_key: FeatureKey,
}

pub struct RevokeEntitlement {
    tx_manager: TxManager,
    entitlements: Arc<dyn EntitlementRepository>,
    cache: Arc<dyn EntitlementCache>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl RevokeEntitlement {
    pub fn new(
        tx_manager: TxManager,
        entitlements: Arc<dyn EntitlementRepository>,
        cache: Arc<dyn EntitlementCache>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            entitlements,
            cache,
            events,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        cmd: RevokeEntitlementCommand,
    ) -> Result<(), AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("revoke_entitlement", "platform-admin")
            .await
            .map_err(storage)?;
        self.entitlements
            .delete(
                &mut scope,
                cmd.tenant_id,
                &cmd.feature_key,
                EntitlementSource::Override,
            )
            .await?;
        self.events
            .publish(
                &mut scope,
                &[AccessEvent::EntitlementRevoked {
                    tenant_id: cmd.tenant_id,
                    feature_key: cmd.feature_key.to_string(),
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
                    tenant_id: Some(cmd.tenant_id),
                    action: "access.entitlement_revoked".to_string(),
                    resource_type: Some("entitlement".to_string()),
                    resource_id: Some(cmd.feature_key.to_string()),
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

        self.cache.invalidate(cmd.tenant_id).await;
        Ok(())
    }
}

/// The tenant's own resolved entitlements (the `GET /api/v1/tenant/entitlements`
/// read model).
pub struct ListTenantEntitlements {
    resolver: Arc<EntitlementResolver>,
}

impl ListTenantEntitlements {
    pub fn new(resolver: Arc<EntitlementResolver>) -> Self {
        Self { resolver }
    }

    pub async fn execute(&self, tenant_id: TenantId) -> Result<Vec<ResolvedEntitlement>, AccessError> {
        let set = self.resolver.resolve(tenant_id).await?;
        Ok(match set {
            crate::application::feature_gate::ResolvedSet::Fresh(entries)
            | crate::application::feature_gate::ResolvedSet::Cached(entries) => entries,
        })
    }
}

/// The operator's view of a tenant's stored override rows.
pub struct ListTenantOverrides {
    tx_manager: TxManager,
    entitlements: Arc<dyn EntitlementRepository>,
}

impl ListTenantOverrides {
    pub fn new(tx_manager: TxManager, entitlements: Arc<dyn EntitlementRepository>) -> Self {
        Self {
            tx_manager,
            entitlements,
        }
    }

    pub async fn execute(&self, tenant_id: TenantId) -> Result<Vec<Entitlement>, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_entitlement_overrides", "platform-admin")
            .await
            .map_err(storage)?;
        let rows = self
            .entitlements
            .list_for_tenant_platform(&mut scope, tenant_id)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(rows
            .into_iter()
            .filter(|row| row.source() == EntitlementSource::Override)
            .collect())
    }
}

/// Sweep expired overrides: delete them, publish `EntitlementExpired` and
/// invalidate the affected tenants' caches. Runs as a worker job.
pub struct ExpireEntitlements {
    tx_manager: TxManager,
    entitlements: Arc<dyn EntitlementRepository>,
    cache: Arc<dyn EntitlementCache>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl ExpireEntitlements {
    pub fn new(
        tx_manager: TxManager,
        entitlements: Arc<dyn EntitlementRepository>,
        cache: Arc<dyn EntitlementCache>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            entitlements,
            cache,
            events,
            clock,
        }
    }

    /// Process up to `limit` expired rows. Returns how many expired.
    pub async fn execute(&self, limit: u32) -> Result<usize, AccessError> {
        let now = self.clock.now();
        let mut scope = self
            .tx_manager
            .begin_unscoped("expire_entitlements", "access")
            .await
            .map_err(storage)?;
        let expired = self.entitlements.expired(&mut scope, now, limit).await?;
        let count = expired.len();
        for row in &expired {
            self.entitlements
                .delete(
                    &mut scope,
                    row.tenant_id(),
                    row.feature(),
                    EntitlementSource::Override,
                )
                .await?;
        }
        let events: Vec<AccessEvent> = expired
            .iter()
            .map(|row| AccessEvent::EntitlementExpired {
                tenant_id: row.tenant_id(),
                feature_key: row.feature().to_string(),
            })
            .collect();
        if !events.is_empty() {
            self.events.publish(&mut scope, &events).await?;
        }
        scope.commit().await.map_err(storage)?;

        for row in &expired {
            self.cache.invalidate(row.tenant_id()).await;
        }
        Ok(count)
    }
}
