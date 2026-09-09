//! Invitations: invite by email, accept (token), resend, revoke, list.

use std::sync::Arc;

use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::TxManager;
use serde::{Deserialize, Serialize};
use tracing::info;
use utoipa::ToSchema;

use crate::application::delegation::DelegationPolicy;
use crate::application::storage;
use crate::domain::events::IdentityEvent;
use crate::domain::invitation::{Invitation, InvitationId, InvitationToken};
use crate::domain::membership::Membership;
use crate::domain::user::UserId;
use crate::domain::values::Email;
use crate::domain::{
    Clock, EventPublisher, IdGenerator, IdentityError, InvitationRepository, MembershipRepository,
    Notifications, RoleCatalog,
};

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InviteMemberCommand {
    #[schema(example = "bob@example.com")]
    pub email: String,
    #[schema(value_type = Vec<String>)]
    pub role_ids: Vec<RoleId>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AcceptInvitationCommand {
    #[schema(example = "64-hex-chars")]
    pub token: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InvitationView {
    #[schema(example = "0190f0d0-0000-7000-8000-000000000001")]
    pub invitation_id: String,
    #[schema(example = "bob@example.com")]
    pub email: String,
    #[schema(value_type = Vec<String>)]
    pub role_ids: Vec<RoleId>,
    pub status: InvitationStatus,
    #[schema(value_type = String)]
    pub expires_at: chrono::DateTime<chrono::Utc>,
    #[schema(value_type = Option<String>)]
    pub accepted_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvitationStatus {
    Pending,
    Accepted,
}

pub struct InviteMember {
    tx_manager: TxManager,
    invitations: Arc<dyn InvitationRepository>,
    memberships: Arc<dyn MembershipRepository>,
    roles: Arc<dyn RoleCatalog>,
    events: Arc<dyn EventPublisher>,
    notifications: Arc<dyn Notifications>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    ttl_days: i64,
    protected_role_key: String,
}

impl InviteMember {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        invitations: Arc<dyn InvitationRepository>,
        memberships: Arc<dyn MembershipRepository>,
        roles: Arc<dyn RoleCatalog>,
        events: Arc<dyn EventPublisher>,
        notifications: Arc<dyn Notifications>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        ttl_days: i64,
    ) -> Self {
        Self {
            tx_manager,
            invitations,
            memberships,
            roles,
            events,
            notifications,
            ids,
            clock,
            ttl_days,
            protected_role_key: "tenant_owner".to_string(),
        }
    }

    pub fn with_protected_role_key(mut self, key: impl Into<String>) -> Self {
        self.protected_role_key = key.into();
        self
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        cmd: InviteMemberCommand,
        actor: UserId,
    ) -> Result<(InvitationView, String), IdentityError> {
        let email = Email::parse(&cmd.email)?;
        let assignable =
            DelegationPolicy::validate_assignable(tenant_id, &cmd.role_ids, &*self.roles).await?;

        let now = self.clock.now();
        let token = InvitationToken::generate();
        let invitation = Invitation::new(
            self.ids.next_invitation_id()?,
            tenant_id,
            email.clone(),
            cmd.role_ids.clone(),
            &token,
            Some(actor),
            now,
            now + chrono::Duration::days(self.ttl_days),
        )?;

        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;

        DelegationPolicy::check_invite(
            &mut scope,
            tenant_id,
            actor,
            &cmd.role_ids,
            &assignable,
            &self.protected_role_key,
            &*self.memberships,
        )
        .await?;

        self.invitations.save(&mut scope, &invitation).await?;
        let events = vec![IdentityEvent::InvitationSent {
            invitation_id: invitation.id(),
            tenant_id,
            email: email.clone(),
        }];
        self.events.publish_scoped(&mut scope, &events).await?;
        scope.commit().await.map_err(storage)?;

        let invite_url = format!("https://invalid.local/invitations/{}", token.as_str());
        let _ = self
            .notifications
            .send_invitation(tenant_id, &email, "tenant", &invite_url)
            .await;
        info!(tenant_id = %tenant_id, email = %email, "invitation sent");
        Ok((
            InvitationView {
                invitation_id: invitation.id().to_string(),
                email: email.to_string(),
                role_ids: invitation.role_ids().to_vec(),
                status: InvitationStatus::Pending,
                expires_at: invitation.expires_at(),
                accepted_at: None,
            },
            token.as_str().to_string(),
        ))
    }
}

pub struct AcceptInvitation {
    tx_manager: TxManager,
    invitations: Arc<dyn InvitationRepository>,
    memberships: Arc<dyn MembershipRepository>,
    roles: Arc<dyn RoleCatalog>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    protected_role_key: String,
}

impl AcceptInvitation {
    pub fn new(
        tx_manager: TxManager,
        invitations: Arc<dyn InvitationRepository>,
        memberships: Arc<dyn MembershipRepository>,
        roles: Arc<dyn RoleCatalog>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            invitations,
            memberships,
            roles,
            events,
            clock,
            protected_role_key: "tenant_owner".to_string(),
        }
    }

    pub fn with_protected_role_key(mut self, key: impl Into<String>) -> Self {
        self.protected_role_key = key.into();
        self
    }

    /// Accept with the signed-in user's identity: the invitation's email
    /// must match the session's email, and the token is single-use.
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        cmd: AcceptInvitationCommand,
        user_id: UserId,
        email: &Email,
    ) -> Result<(), IdentityError> {
        let token = InvitationToken::parse(&cmd.token)?;
        let token_hash = token.token_hash();

        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        let mut invitation = self
            .invitations
            .find_by_token_hash(&mut scope, tenant_id, &token_hash)
            .await?
            .ok_or(IdentityError::InvitationNotFound {
                id: InvitationId::try_from(1_i64).expect("seed invitation id"),
            })?;
        let now = self.clock.now();
        invitation.validate_acceptance(email, now)?;

        // Roles may have been deleted between invite and accept.
        let assignable = self.roles.assignable_roles(tenant_id).await?;
        let assignable_ids: std::collections::BTreeSet<RoleId> =
            assignable.iter().map(|(id, _)| id).copied().collect();
        if let Some(unknown) = invitation
            .role_ids()
            .iter()
            .find(|id| !assignable_ids.contains(id))
        {
            return Err(IdentityError::UnknownRole(*unknown));
        }

        DelegationPolicy::check_accept(
            &mut scope,
            tenant_id,
            invitation.invited_by(),
            invitation.role_ids(),
            &assignable,
            &self.protected_role_key,
            &*self.memberships,
        )
        .await?;

        let mut events = Vec::new();
        if let Some(existing) = self
            .memberships
            .find(&mut scope, tenant_id, user_id)
            .await?
        {
            // Already a member: merge the invitation's roles in.
            let mut merged = existing.role_ids().to_vec();
            for role in invitation.role_ids() {
                if !merged.contains(role) {
                    merged.push(*role);
                }
            }
            let mut membership = existing;
            let (_added, removed) = membership.replace_roles(merged)?;
            let version = self.memberships.save(&mut scope, &membership).await?;
            events.push(IdentityEvent::MembershipRolesChanged {
                tenant_id,
                user_id,
                role_ids: membership.role_ids().to_vec(),
                removed_role_ids: removed,
                version,
            });
        } else {
            let membership = Membership::grant(
                tenant_id,
                user_id,
                invitation.role_ids().to_vec(),
                invitation.invited_by(),
                now,
            )?;
            let version = self.memberships.save(&mut scope, &membership).await?;
            events.push(IdentityEvent::MembershipGranted {
                tenant_id,
                user_id,
                role_ids: membership.role_ids().to_vec(),
                version,
            });
        }

        invitation.mark_accepted(now)?;
        self.invitations.save(&mut scope, &invitation).await?;
        events.push(IdentityEvent::InvitationAccepted {
            invitation_id: invitation.id(),
            tenant_id,
            user_id,
        });
        self.events.publish_scoped(&mut scope, &events).await?;
        scope.commit().await.map_err(storage)?;
        info!(tenant_id = %tenant_id, user_id = %user_id, "invitation accepted");
        Ok(())
    }
}

pub struct ResendInvitation {
    tx_manager: TxManager,
    invitations: Arc<dyn InvitationRepository>,
    events: Arc<dyn EventPublisher>,
    notifications: Arc<dyn Notifications>,
    clock: Arc<dyn Clock>,
    ttl_days: i64,
}

impl ResendInvitation {
    pub fn new(
        tx_manager: TxManager,
        invitations: Arc<dyn InvitationRepository>,
        events: Arc<dyn EventPublisher>,
        notifications: Arc<dyn Notifications>,
        clock: Arc<dyn Clock>,
        ttl_days: i64,
    ) -> Self {
        Self {
            tx_manager,
            invitations,
            events,
            notifications,
            clock,
            ttl_days,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        invitation_id: InvitationId,
    ) -> Result<InvitationView, IdentityError> {
        let now = self.clock.now();
        let token = InvitationToken::generate();

        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        let mut invitation = self
            .invitations
            .find_by_id(&mut scope, tenant_id, invitation_id)
            .await?
            .ok_or(IdentityError::InvitationNotFound { id: invitation_id })?;
        invitation.reissue(&token, now, now + chrono::Duration::days(self.ttl_days))?;
        self.invitations.save(&mut scope, &invitation).await?;
        let events = vec![IdentityEvent::InvitationSent {
            invitation_id: invitation.id(),
            tenant_id,
            email: invitation.email().clone(),
        }];
        self.events.publish_scoped(&mut scope, &events).await?;
        scope.commit().await.map_err(storage)?;

        let invite_url = format!("https://invalid.local/invitations/{}", token.as_str());
        let _ = self
            .notifications
            .send_invitation(tenant_id, invitation.email(), "tenant", &invite_url)
            .await;
        Ok(InvitationView {
            invitation_id: invitation.id().to_string(),
            email: invitation.email().to_string(),
            role_ids: invitation.role_ids().to_vec(),
            status: InvitationStatus::Pending,
            expires_at: invitation.expires_at(),
            accepted_at: None,
        })
    }
}

pub struct RevokeInvitation {
    tx_manager: TxManager,
    invitations: Arc<dyn InvitationRepository>,
}

impl RevokeInvitation {
    pub fn new(tx_manager: TxManager, invitations: Arc<dyn InvitationRepository>) -> Self {
        Self {
            tx_manager,
            invitations,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        invitation_id: InvitationId,
    ) -> Result<(), IdentityError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        self.invitations
            .remove(&mut scope, tenant_id, invitation_id)
            .await?;
        scope.commit().await.map_err(storage)?;
        info!(tenant_id = %tenant_id, invitation_id = %invitation_id, "invitation revoked");
        Ok(())
    }
}

pub struct ListInvitations {
    tx_manager: TxManager,
    invitations: Arc<dyn InvitationRepository>,
}

impl ListInvitations {
    pub fn new(tx_manager: TxManager, invitations: Arc<dyn InvitationRepository>) -> Self {
        Self {
            tx_manager,
            invitations,
        }
    }

    pub async fn execute(&self, tenant_id: TenantId) -> Result<Vec<InvitationView>, IdentityError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        let invitations = self.invitations.list(&mut scope, tenant_id).await?;
        scope.commit().await.map_err(storage)?;
        Ok(invitations
            .into_iter()
            .map(|invitation| InvitationView {
                invitation_id: invitation.id().to_string(),
                email: invitation.email().to_string(),
                role_ids: invitation.role_ids().to_vec(),
                status: if invitation.accepted_at().is_some() {
                    InvitationStatus::Accepted
                } else {
                    InvitationStatus::Pending
                },
                expires_at: invitation.expires_at(),
                accepted_at: invitation.accepted_at(),
            })
            .collect())
    }
}
