//! The signed-in user's self profile.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::application::storage;
use crate::domain::user::{UserId, UserStatus};
use crate::domain::{IdentityError, UserRepository};

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
}

pub struct GetMe {
    tx_manager: TxManager,
    users: Arc<dyn UserRepository>,
}

impl GetMe {
    pub fn new(tx_manager: TxManager, users: Arc<dyn UserRepository>) -> Self {
        Self { tx_manager, users }
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
        scope.commit().await.map_err(storage)?;
        Ok(MeView {
            user_id: user.id().to_string(),
            email: user.primary_email().to_string(),
            email_verified: user.email_verified(),
            locale: user.locale().map(|locale| locale.as_str().to_string()),
            status: user.status(),
            created_at: user.created_at(),
        })
    }
}
