//! Impersonation use cases: create (with guardrails), list, revoke.
//!
//! Creation validates: the target user is an active member of the tenant,
//! the tenant has no privacy lock, the duration is within the configured
//! ceiling, and a reason is recorded. Every mutation writes an audit entry
//! and publishes an `ImpersonationStarted/Ended` event in the same
//! transaction as the grant change.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_identity::domain::MembershipRepository;
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::application::storage;
use crate::application::tenant_views::MAX_PAGE_LIMIT;
use crate::domain::PlatformAdminError;
use crate::domain::events::PlatformAdminEvent;
use crate::domain::impersonation::{ImpersonationGrant, ImpersonationGrantId, ImpersonationMode};
use crate::domain::ports::{
    Clock, EventPublisher, IdGenerator, ImpersonationGrantRepository, PrivacyLock,
};

#[derive(Debug, Clone)]
pub struct CreateImpersonationCommand {
    pub tenant_id: TenantId,
    pub target_user_id: UserId,
    pub reason: String,
    pub mode: ImpersonationMode,
    pub duration_minutes: u32,
}

/// One grant as exposed by the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ImpersonationGrantView {
    #[schema(value_type = String)]
    pub id: ImpersonationGrantId,
    #[schema(value_type = String)]
    pub actor_id: UserId,
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    #[schema(value_type = String)]
    pub target_user_id: UserId,
    pub reason: String,
    pub mode: ImpersonationMode,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub revoked_by: Option<UserId>,
}

impl From<&ImpersonationGrant> for ImpersonationGrantView {
    fn from(grant: &ImpersonationGrant) -> Self {
        Self {
            id: grant.id(),
            actor_id: grant.actor_id(),
            tenant_id: grant.tenant_id(),
            target_user_id: grant.target_user_id(),
            reason: grant.reason().to_string(),
            mode: grant.mode(),
            created_at: grant.created_at(),
            expires_at: grant.expires_at(),
            revoked_at: grant.revoked_at(),
            revoked_by: grant.revoked_by(),
        }
    }
}

pub struct CreateImpersonation {
    tx_manager: TxManager,
    grants: Arc<dyn ImpersonationGrantRepository>,
    memberships: Arc<dyn MembershipRepository>,
    privacy_lock: Arc<dyn PrivacyLock>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
    max_minutes: u32,
}

impl CreateImpersonation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        grants: Arc<dyn ImpersonationGrantRepository>,
        memberships: Arc<dyn MembershipRepository>,
        privacy_lock: Arc<dyn PrivacyLock>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
        max_minutes: u32,
    ) -> Self {
        Self {
            tx_manager,
            grants,
            memberships,
            privacy_lock,
            events,
            ids,
            clock,
            audit,
            max_minutes,
        }
    }

    #[tracing::instrument(name = "platform_admin.create_impersonation", skip_all, fields(tenant_id = ?cmd.tenant_id))]
    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        cmd: CreateImpersonationCommand,
    ) -> Result<ImpersonationGrantView, PlatformAdminError> {
        // Impersonation into a tenant with an active privacy lock is
        // refused outright (phase 05 wires the real check).
        if self.privacy_lock.is_locked(cmd.tenant_id).await? {
            return Err(PlatformAdminError::PrivacyLocked {
                tenant_id: cmd.tenant_id,
            });
        }

        // The target must be an active member of the tenant.
        let membership = {
            let mut scope = self
                .tx_manager
                .begin_for(cmd.tenant_id.as_i64())
                .await
                .map_err(storage)?;
            let membership = self
                .memberships
                .find(&mut scope, cmd.tenant_id, cmd.target_user_id)
                .await
                .map_err(storage)?;
            scope.commit().await.map_err(storage)?;
            membership
        }
        .ok_or(PlatformAdminError::TargetNotMember {
            user_id: cmd.target_user_id,
            tenant_id: cmd.tenant_id,
        })?;
        if membership.status() != sanvi_identity::domain::MembershipStatus::Active {
            return Err(PlatformAdminError::TargetNotMember {
                user_id: cmd.target_user_id,
                tenant_id: cmd.tenant_id,
            });
        }

        let now = self.clock.now();
        let grant = ImpersonationGrant::new(
            self.ids.next_grant_id()?,
            actor,
            cmd.tenant_id,
            cmd.target_user_id,
            cmd.reason.clone(),
            cmd.mode,
            cmd.duration_minutes,
            self.max_minutes,
            now,
        )?;

        let mut scope = self
            .tx_manager
            .begin_unscoped("create_impersonation", "platform-admin")
            .await
            .map_err(storage)?;
        self.grants.save(&mut scope, &grant).await?;
        self.events
            .publish(
                &mut scope,
                &[PlatformAdminEvent::ImpersonationStarted {
                    grant_id: grant.id(),
                    actor_id: actor,
                    tenant_id: cmd.tenant_id,
                    target_user_id: cmd.target_user_id,
                    mode: cmd.mode,
                    reason: cmd.reason,
                    expires_at: grant.expires_at(),
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
                    action: "platform_admin.impersonation_started".to_string(),
                    resource_type: Some("impersonation".to_string()),
                    resource_id: Some(grant.id().to_string()),
                    before: None,
                    after: Some(serde_json::json!({
                        "target_user_id": cmd.target_user_id,
                        "mode": cmd.mode.as_str(),
                        "expires_at": grant.expires_at(),
                        "reason": grant.reason(),
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(storage)?;
        scope.commit().await.map_err(storage)?;

        Ok(ImpersonationGrantView::from(&grant))
    }
}

pub struct ListImpersonations {
    tx_manager: TxManager,
    grants: Arc<dyn ImpersonationGrantRepository>,
    clock: Arc<dyn Clock>,
}

impl ListImpersonations {
    pub fn new(
        tx_manager: TxManager,
        grants: Arc<dyn ImpersonationGrantRepository>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            grants,
            clock,
        }
    }

    #[tracing::instrument(name = "platform_admin.list_impersonations", skip_all, fields(tenant_id = ?tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: Option<TenantId>,
        limit: Option<u32>,
        after_id: Option<i64>,
    ) -> Result<Vec<ImpersonationGrantView>, PlatformAdminError> {
        let limit = sanvi_kernel::pagination::clamp_page_size(limit, 100, MAX_PAGE_LIMIT);
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_impersonations", "platform-admin")
            .await
            .map_err(storage)?;
        let grants = self
            .grants
            .list_active(&mut scope, tenant_id, self.clock.now(), limit, after_id)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(grants.iter().map(ImpersonationGrantView::from).collect())
    }
}

pub struct RevokeImpersonation {
    tx_manager: TxManager,
    grants: Arc<dyn ImpersonationGrantRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl RevokeImpersonation {
    pub fn new(
        tx_manager: TxManager,
        grants: Arc<dyn ImpersonationGrantRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            grants,
            events,
            clock,
            audit,
        }
    }

    /// Revoke a grant. Immediate: the middleware validates per request, so
    /// the next exchange fails. Idempotent.
    #[tracing::instrument(name = "platform_admin.revoke_impersonation", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        on_behalf_of: Option<UserId>,
        grant_id: ImpersonationGrantId,
    ) -> Result<ImpersonationGrantView, PlatformAdminError> {
        let now = self.clock.now();
        let mut scope = self
            .tx_manager
            .begin_unscoped("revoke_impersonation", "platform-admin")
            .await
            .map_err(storage)?;
        let mut grant = self
            .grants
            .find_by_id(&mut scope, grant_id)
            .await?
            .ok_or(PlatformAdminError::GrantNotFound { id: grant_id })?;
        grant.revoke(actor, now);
        self.grants.save(&mut scope, &grant).await?;
        self.events
            .publish(
                &mut scope,
                &[PlatformAdminEvent::ImpersonationEnded {
                    grant_id: grant.id(),
                    actor_id: grant.actor_id(),
                    tenant_id: grant.tenant_id(),
                    target_user_id: grant.target_user_id(),
                    ended_by: actor,
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
                    tenant_id: Some(grant.tenant_id()),
                    action: "platform_admin.impersonation_revoked".to_string(),
                    resource_type: Some("impersonation".to_string()),
                    resource_id: Some(grant.id().to_string()),
                    before: None,
                    after: None,
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(storage)?;
        scope.commit().await.map_err(storage)?;
        Ok(ImpersonationGrantView::from(&grant))
    }
}
