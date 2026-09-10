//! Session self-management: list (the current session via whoami) and
//! revoke. Cross-session revocation is a platform-admin concern (phase 03);
//! here a user can only manage the session they are currently using.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::application::storage;
use crate::domain::events::IdentityEvent;
use crate::domain::{
    EventPublisher, IdentityError, KratosClient, Notifications, SessionCache, UserRepository,
};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SessionView {
    pub session_id: String,
    pub aal: String,
    pub methods: Vec<String>,
    #[schema(value_type = Option<String>)]
    pub authenticated_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct ListSessions {
    kratos: Arc<dyn KratosClient>,
}

impl ListSessions {
    pub fn new(kratos: Arc<dyn KratosClient>) -> Self {
        Self { kratos }
    }

    /// The current session, from Kratos (the cache is not a source of truth
    /// for anything except subject resolution).
    pub async fn execute(
        &self,
        session_token: &str,
        channel: sanvi_http::SessionChannel,
    ) -> Result<Vec<SessionView>, IdentityError> {
        let session = self
            .kratos
            .whoami(session_token, channel)
            .await?
            .ok_or(IdentityError::SessionNotFound)?;
        Ok(vec![SessionView {
            session_id: session.session_id,
            aal: session.aal,
            methods: session.methods,
            authenticated_at: session.authenticated_at,
        }])
    }
}

pub struct RevokeSession {
    tx_manager: TxManager,
    kratos: Arc<dyn KratosClient>,
    users: Arc<dyn UserRepository>,
    events: Arc<dyn EventPublisher>,
    #[allow(dead_code)]
    notifications: Arc<dyn Notifications>,
    cache: Arc<dyn SessionCache>,
}

impl RevokeSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        kratos: Arc<dyn KratosClient>,
        users: Arc<dyn UserRepository>,
        events: Arc<dyn EventPublisher>,
        notifications: Arc<dyn Notifications>,
        cache: Arc<dyn SessionCache>,
    ) -> Self {
        Self {
            tx_manager,
            kratos,
            users,
            events,
            notifications,
            cache,
        }
    }

    /// Revoke the *current* session: the token must belong to the session
    /// being revoked (Kratos enforces this too; we refuse mismatches).
    pub async fn execute(
        &self,
        user_id: crate::domain::UserId,
        session_id: &str,
        session_token: &str,
        channel: sanvi_http::SessionChannel,
    ) -> Result<(), IdentityError> {
        let session = self
            .kratos
            .whoami(session_token, channel)
            .await?
            .ok_or(IdentityError::SessionNotFound)?;
        if session.session_id != session_id {
            // Never allow revoking another session of the same user: that is
            // a settings-flow/phase-03 capability.
            return Err(IdentityError::SessionNotFound);
        }
        self.kratos.revoke_session(session_id).await?;
        // Drop the cached verification immediately: the TTL backstop exists,
        // but a revoked session must not stay usable here.
        if let Err(err) = self.cache.invalidate_session(session_id).await {
            tracing::warn!(error = %err, session_id, "session cache invalidation failed");
        }

        let mut scope = self
            .tx_manager
            .begin_unscoped("revoke_session", "identity")
            .await
            .map_err(storage)?;
        let user = self.users.find_by_id(&mut scope, user_id).await?;
        let email = user.as_ref().map(|u| u.primary_email().clone());
        let events = vec![IdentityEvent::SessionRevoked {
            user_id: Some(user_id),
            session_id: session_id.to_string(),
            email,
        }];
        self.events.publish(&mut scope, &events).await?;
        scope.commit().await.map_err(storage)?;
        Ok(())
    }
}
