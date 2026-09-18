//! List tenants with cursor pagination.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::application::get_tenant::TenantView;
use crate::domain::tenant::{TenantId, TenantStatus};
use crate::domain::{TenancyError, TenantRepository};

use super::storage;

const ACTOR: &str = "platform-admin";

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct TenantListView {
    pub items: Vec<TenantView>,
    /// Cursor for the next page (null on the last page).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

pub struct ListTenants {
    tx_manager: TxManager,
    tenants: Arc<dyn TenantRepository>,
}

impl ListTenants {
    pub fn new(tx_manager: TxManager, tenants: Arc<dyn TenantRepository>) -> Self {
        Self {
            tx_manager,
            tenants,
        }
    }

    #[tracing::instrument(name = "tenancy.list_tenants", skip_all)]
    pub async fn execute(
        &self,
        after: Option<TenantId>,
        limit: u32,
        status: Option<TenantStatus>,
    ) -> Result<TenantListView, TenancyError> {
        let limit = limit.clamp(1, 100);
        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_list", ACTOR)
            .await
            .map_err(storage)?;
        let (tenants, next_cursor) = self.tenants.list(&mut scope, after, limit, status).await?;
        scope.commit().await.map_err(storage)?;
        Ok(TenantListView {
            items: tenants.iter().map(TenantView::from).collect(),
            next_cursor: next_cursor.map(|id| id.to_string()),
        })
    }
}
