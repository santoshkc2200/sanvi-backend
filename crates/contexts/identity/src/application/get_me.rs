//! The signed-in user's self profile.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::application::storage;
use crate::domain::membership::MembershipStatus;
use crate::domain::user::{UserId, UserStatus};
use crate::domain::{
    IdentityError, MembershipRepository, RoleCatalog, TenantCatalog, UserRepository,
};

/// One tenant `GET /api/v1/me` reports the caller as a member of, with the
/// roles/permissions they hold there — the single hydration call the
/// frontend's session store and `can()`/`<Can>` are built on (see
/// `sanvi-frontend/docs/phase-02-auth-ux`), so it carries everything a tenant
/// switcher and permission-aware render need without a per-tenant follow-up
/// request.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MembershipSummary {
    #[schema(example = "0190f0d0-0000-7000-8000-000000000002")]
    pub tenant_id: String,
    #[schema(example = "acme")]
    pub tenant_slug: String,
    #[schema(example = "Acme Corporation")]
    pub tenant_name: String,
    pub role_ids: Vec<String>,
    /// The union of every permission `role_ids` grants — a UX hint only; the
    /// backend enforces every mutation independently of what this list says.
    pub permissions: Vec<String>,
    pub status: MembershipStatus,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MeView {
    #[schema(example = "0190f0d0-0000-7000-8000-000000000001")]
    pub user_id: String,
    #[schema(example = "alice@example.com")]
    pub email: String,
    pub email_verified: bool,
    #[schema(example = "en")]
    pub locale: Option<String>,
    pub status: UserStatus,
    #[schema(value_type = String)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub memberships: Vec<MembershipSummary>,
}

pub struct GetMe {
    tx_manager: TxManager,
    users: Arc<dyn UserRepository>,
    memberships: Arc<dyn MembershipRepository>,
    role_catalog: Arc<dyn RoleCatalog>,
    tenant_catalog: Arc<dyn TenantCatalog>,
}

impl GetMe {
    pub fn new(
        tx_manager: TxManager,
        users: Arc<dyn UserRepository>,
        memberships: Arc<dyn MembershipRepository>,
        role_catalog: Arc<dyn RoleCatalog>,
        tenant_catalog: Arc<dyn TenantCatalog>,
    ) -> Self {
        Self {
            tx_manager,
            users,
            memberships,
            role_catalog,
            tenant_catalog,
        }
    }

    pub async fn execute(&self, user_id: UserId) -> Result<MeView, IdentityError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("get_me", "identity")
            .await
            .map_err(storage)?;
        let user = self
            .users
            .find_by_id(&mut scope, user_id)
            .await?
            .ok_or(IdentityError::UserNotFound { id: user_id })?;
        let memberships = self
            .memberships
            .list_for_user_platform(&mut scope, user_id)
            .await?;
        scope.commit().await.map_err(storage)?;

        let tenant_ids: Vec<_> = memberships.iter().map(|m| m.tenant_id()).collect();
        let tenants: std::collections::HashMap<_, _> = self
            .tenant_catalog
            .summaries(&tenant_ids)
            .await?
            .into_iter()
            .map(|tenant| (tenant.tenant_id, tenant))
            .collect();

        let mut membership_summaries = Vec::with_capacity(memberships.len());
        for membership in memberships {
            // A membership referencing a since-deleted tenant is skipped
            // rather than surfaced with blank metadata — it's cleaned up by
            // the same erasure/reconciliation path that handles the rest of
            // a deleted tenant's rows.
            let Some(tenant) = tenants.get(&membership.tenant_id()) else {
                continue;
            };
            let permissions = self
                .role_catalog
                .permissions_for_roles(membership.tenant_id(), membership.role_ids())
                .await?;
            membership_summaries.push(MembershipSummary {
                tenant_id: tenant.tenant_id.to_string(),
                tenant_slug: tenant.slug.clone(),
                tenant_name: tenant.name.clone(),
                role_ids: membership
                    .role_ids()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                permissions,
                status: membership.status(),
            });
        }

        Ok(MeView {
            user_id: user.id().to_string(),
            email: user.primary_email().to_string(),
            email_verified: user.email_verified(),
            locale: user.locale().map(|locale| locale.as_str().to_string()),
            status: user.status(),
            created_at: user.created_at(),
            memberships: membership_summaries,
        })
    }
}
