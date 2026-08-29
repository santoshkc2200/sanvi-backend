//! The `sanvi_http::SessionVerifier` implementation: Kratos whoami (cached)
//! → local user projection → subject with the current tenant's roles.

use std::pin::Pin;
use std::sync::Arc;

use sanvi_http::{ApiError, SessionVerifier, Subject};
use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::TxManager;
use sha2::{Digest, Sha256};

use crate::domain::{
    IdentityError, KratosClient, MembershipRepository, SessionCache, UserRepository,
};

pub struct KratosSessionVerifier {
    kratos: Arc<dyn KratosClient>,
    cache: Arc<dyn SessionCache>,
    users: Arc<dyn UserRepository>,
    memberships: Arc<dyn MembershipRepository>,
    tx_manager: TxManager,
    cache_ttl_secs: u64,
}

impl KratosSessionVerifier {
    pub fn new(
        kratos: Arc<dyn KratosClient>,
        cache: Arc<dyn SessionCache>,
        users: Arc<dyn UserRepository>,
        memberships: Arc<dyn MembershipRepository>,
        tx_manager: TxManager,
        cache_ttl_secs: u64,
    ) -> Self {
        Self {
            kratos,
            cache,
            users,
            memberships,
            tx_manager,
            cache_ttl_secs,
        }
    }

    /// Look up the local user row and (tenant-scoped) role ids.
    async fn resolve_user(
        &self,
        identity_id: uuid::Uuid,
    ) -> Result<Option<crate::domain::User>, ApiError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("session_verify", "identity")
            .await
            .map_err(|err| ApiError::internal(format!("storage failure: {err}")))?;
        let user = self
            .users
            .find_by_kratos(&mut scope, identity_id)
            .await
            .map_err(backend)?;
        scope
            .commit()
            .await
            .map_err(|err| ApiError::internal(format!("storage failure: {err}")))?;
        Ok(user)
    }

    async fn roles_in(
        &self,
        tenant_id: TenantId,
        user_id: crate::domain::UserId,
    ) -> Result<Vec<RoleId>, ApiError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| ApiError::internal(format!("storage failure: {err}")))?;
        let roles = match self.memberships.find(&mut scope, tenant_id, user_id).await {
            Ok(Some(membership)) => membership.role_ids().to_vec(),
            Ok(None) => Vec::new(),
            Err(err) => return Err(backend(err)),
        };
        scope
            .commit()
            .await
            .map_err(|err| ApiError::internal(format!("storage failure: {err}")))?;
        Ok(roles)
    }
}

fn backend(err: IdentityError) -> ApiError {
    match err {
        IdentityError::KratosUnreachable(detail) | IdentityError::KratosRejected(detail) => {
            tracing::error!(error = %detail, "kratos backend failure during session verification");
            ApiError::ServiceUnavailable {
                detail: "authentication is temporarily unavailable".to_string(),
            }
        }
        other => ApiError::internal(other.to_string()),
    }
}

fn token_hash(session_token: &str) -> String {
    let digest = Sha256::digest(session_token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl SessionVerifier for KratosSessionVerifier {
    fn verify<'a>(
        &'a self,
        session_token: Option<(&'a str, sanvi_http::SessionChannel)>,
        tenant_id: Option<TenantId>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<Subject>, ApiError>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some((session_token, channel)) = session_token else {
                return Ok(None);
            };
            let hash = token_hash(session_token);

            // 1. Cache hit: skip Kratos entirely.
            let session = match self.cache.get(&hash).await {
                Ok(Some(session)) => Some(session),
                Ok(None) => {
                    // 2. Kratos whoami; cache whatever we learn (including
                    //    absence, implicitly by not caching).
                    let verified = self
                        .kratos
                        .whoami(session_token, channel)
                        .await
                        .map_err(backend)?;
                    if let Some(session) = &verified
                        && let Err(err) = self.cache.put(&hash, session, self.cache_ttl_secs).await
                    {
                        tracing::warn!(error = %err, "session cache write failed");
                    }
                    verified
                }
                Err(err) => {
                    // Cache trouble: degrade to Kratos, never fail open to
                    // "no session" and never fail closed to 503 — the cache
                    // is an optimisation.
                    tracing::warn!(error = %err, "session cache read failed; verifying via kratos");
                    self.kratos
                        .whoami(session_token, channel)
                        .await
                        .map_err(backend)?
                }
            };

            let Some(session) = session else {
                return Ok(None);
            };

            // 3. Local projection: an identity the webhooks haven't recorded
            //    is not an authenticated user (fail closed, log loudly).
            let Some(user) = self.resolve_user(session.identity_id).await? else {
                tracing::error!(
                    kratos_identity_id = %session.identity_id,
                    "kratos identity has no local projection; rejecting session"
                );
                return Ok(None);
            };
            if let Err(err) = user.ensure_active() {
                tracing::warn!(user_id = %user.id(), error = %err, "deactivated user session rejected");
                return Ok(None);
            }

            let role_ids = match tenant_id {
                Some(tenant_id) => self.roles_in(tenant_id, user.id()).await?,
                None => Vec::new(),
            };

            Ok(Some(Subject {
                user_id: user.id(),
                session_id: session.session_id,
                aal: session.aal,
                methods: session.methods,
                tenant_id,
                role_ids,
                authenticated_at: session.authenticated_at,
            }))
        })
    }
}
