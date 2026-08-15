//! Read one tenant by id.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::tenant::{Tenant, TenantId};
use crate::domain::{TenancyError, TenantRepository};

use super::storage;

/// Public representation of a tenant.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct TenantView {
    pub id: String,
    pub slug: String,
    pub display_name: String,
    pub status: String,
    pub region: String,
    pub default_locale: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suspended_reason: Option<String>,
}

impl From<&Tenant> for TenantView {
    fn from(tenant: &Tenant) -> Self {
        Self {
            id: tenant.id().to_string(),
            slug: tenant.slug().to_string(),
            display_name: tenant.display_name().to_string(),
            status: tenant.status().as_str().to_string(),
            region: tenant.region().to_string(),
            default_locale: tenant.default_locale().to_string(),
            created_at: tenant.created_at(),
            updated_at: tenant.updated_at(),
            archived_at: tenant.archived_at(),
            suspended_reason: tenant.suspended_reason().map(|reason| reason.to_string()),
        }
    }
}

const ACTOR: &str = "platform-admin";

pub struct GetTenant {
    tx_manager: TxManager,
    tenants: Arc<dyn TenantRepository>,
}

impl GetTenant {
    pub fn new(tx_manager: TxManager, tenants: Arc<dyn TenantRepository>) -> Self {
        Self {
            tx_manager,
            tenants,
        }
    }

    pub async fn execute(&self, id: TenantId) -> Result<TenantView, TenancyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_read", ACTOR)
            .await
            .map_err(storage)?;
        let tenant = self
            .tenants
            .find_by_id(&mut scope, id)
            .await?
            .ok_or(TenancyError::TenantNotFound { id: id.to_string() })?;
        scope.commit().await.map_err(storage)?;
        Ok(TenantView::from(&tenant))
    }
}
