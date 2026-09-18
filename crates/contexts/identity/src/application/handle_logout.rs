//! The logout webhook: invalidate the session cache and record the
//! revocation. Fire-and-forget-friendly (idempotent on replay).

use std::sync::Arc;

use sanvi_persistence::TxManager;
use tracing::info;

use crate::application::storage;
use crate::domain::events::IdentityEvent;
use crate::domain::{EventPublisher, IdentityError, SessionCache, UserRepository};

pub struct HandleLogoutWebhook {
    tx_manager: TxManager,
    cache: Arc<dyn SessionCache>,
    users: Arc<dyn UserRepository>,
    events: Arc<dyn EventPublisher>,
}

impl HandleLogoutWebhook {
    pub fn new(
        tx_manager: TxManager,
        cache: Arc<dyn SessionCache>,
        users: Arc<dyn UserRepository>,
        events: Arc<dyn EventPublisher>,
    ) -> Self {
        Self {
            tx_manager,
            cache,
            users,
            events,
        }
    }

    #[tracing::instrument(name = "identity.handle_logout_webhook", skip_all)]
    pub async fn execute(
        &self,
        session_id: Option<String>,
        kratos_identity_id: Option<uuid::Uuid>,
    ) -> Result<(), IdentityError> {
        // Cache invalidation first: replay-safe and independent of the
        // database.
        if let Some(session_id) = &session_id
            && let Err(err) = self.cache.invalidate_session(session_id).await
        {
            tracing::warn!(error = %err, session_id = %session_id, "session cache invalidation failed");
        }

        let mut scope = self
            .tx_manager
            .begin_unscoped("kratos_logout_webhook", "kratos")
            .await
            .map_err(storage)?;

        let user_id = match kratos_identity_id {
            Some(identity_id) => self
                .users
                .find_by_kratos(&mut scope, identity_id)
                .await?
                .map(|user| user.id()),
            None => None,
        };

        if let Some(session_id) = session_id {
            self.events
                .publish(
                    &mut scope,
                    &[IdentityEvent::SessionRevoked {
                        user_id,
                        session_id: session_id.clone(),
                        email: None,
                    }],
                )
                .await?;
            scope.commit().await.map_err(storage)?;
            info!(session_id = %session_id, "kratos session revoked");
        } else {
            scope.commit().await.map_err(storage)?;
        }
        Ok(())
    }
}
