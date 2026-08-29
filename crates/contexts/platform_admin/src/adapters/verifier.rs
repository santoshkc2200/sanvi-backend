//! The `sanvi_http::ImpersonationVerifier` implementation: validates the
//! grant per request (existence, revocation, expiry — so revocation is
//! effective immediately), and resolves the target's current tenant roles
//! through the identity context's membership repository.

use std::pin::Pin;
use std::sync::Arc;

use sanvi_http::{
    ApiError, ImpersonatedSubject, ImpersonationMode as HttpMode, ImpersonationVerifier, Subject,
};
use sanvi_identity::domain::MembershipRepository;
use sanvi_kernel::{SnowflakeId, TenantId};
use sanvi_persistence::TxManager;

use crate::domain::PlatformAdminError;
use crate::domain::impersonation::{ImpersonationGrantId, ImpersonationMode};
use crate::domain::ports::{Clock, ImpersonationGrantRepository};

pub struct SqlxImpersonationVerifier {
    grants: Arc<dyn ImpersonationGrantRepository>,
    memberships: Arc<dyn MembershipRepository>,
    tx_manager: TxManager,
    clock: Arc<dyn Clock>,
}

impl SqlxImpersonationVerifier {
    pub fn new(
        grants: Arc<dyn ImpersonationGrantRepository>,
        memberships: Arc<dyn MembershipRepository>,
        tx_manager: TxManager,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            grants,
            memberships,
            tx_manager,
            clock,
        }
    }
}

fn map_error(err: PlatformAdminError) -> ApiError {
    tracing::error!(error = %err, "impersonation verification backend failure");
    ApiError::internal("impersonation verification failed")
}

impl ImpersonationVerifier for SqlxImpersonationVerifier {
    fn verify<'a>(
        &'a self,
        grant_id: SnowflakeId,
        _actor: &'a Subject,
        _tenant_id: Option<TenantId>,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<ImpersonatedSubject>, ApiError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let grant = {
                let mut scope = self
                    .tx_manager
                    .begin_unscoped("impersonation_verify", "platform-admin")
                    .await
                    .map_err(|err| ApiError::internal(format!("storage failure: {err}")))?;
                let grant = self
                    .grants
                    .find_by_id(&mut scope, ImpersonationGrantId::from_snowflake(grant_id))
                    .await
                    .map_err(map_error)?;
                scope
                    .commit()
                    .await
                    .map_err(|err| ApiError::internal(format!("storage failure: {err}")))?;
                grant
            };
            let Some(grant) = grant else {
                return Ok(None);
            };
            // Expired or revoked: refuse immediately (checked per request,
            // so revocation is effective at once).
            if !grant.is_active(self.clock.now()) {
                return Ok(None);
            }

            // The target must still be an active member with roles in the
            // grant's tenant (membership may have changed since the grant
            // was created).
            let roles = {
                let mut scope = self
                    .tx_manager
                    .begin_for(grant.tenant_id().as_i64())
                    .await
                    .map_err(|err| ApiError::internal(format!("storage failure: {err}")))?;
                let membership = self
                    .memberships
                    .find(&mut scope, grant.tenant_id(), grant.target_user_id())
                    .await
                    .map_err(|err: sanvi_identity::domain::IdentityError| {
                        map_error(PlatformAdminError::Storage(err.to_string()))
                    })?;
                scope
                    .commit()
                    .await
                    .map_err(|err| ApiError::internal(format!("storage failure: {err}")))?;
                membership
            };
            let Some(membership) = roles else {
                return Ok(None);
            };
            if membership.status() != sanvi_identity::domain::MembershipStatus::Active {
                return Ok(None);
            }

            let mode = match grant.mode() {
                ImpersonationMode::ReadOnly => HttpMode::ReadOnly,
                ImpersonationMode::ReadWrite => HttpMode::ReadWrite,
            };
            Ok(Some(ImpersonatedSubject {
                grant_id: grant.id().as_snowflake(),
                actor_user_id: grant.actor_id(),
                target_user_id: grant.target_user_id(),
                tenant_id: grant.tenant_id(),
                role_ids: membership.role_ids().to_vec(),
                mode,
            }))
        })
    }
}
