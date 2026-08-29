//! The operator tenant read model use cases: search + detail. The heavy
//! lifting lives in the `TenantAdminViewRepository` port (assembled over
//! the tenancy/identity/access ports — never cross-schema SQL).

use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::application::storage;
use crate::domain::PlatformAdminError;
use crate::domain::ports::TenantAdminViewRepository;
use crate::domain::views::{TenantAdminView, TenantViewCursor, TenantViewSearch};

pub const DEFAULT_PAGE_LIMIT: u32 = 25;
pub const MAX_PAGE_LIMIT: u32 = 100;

pub struct SearchTenantAdminViews {
    tx_manager: TxManager,
    views: Arc<dyn TenantAdminViewRepository>,
}

impl SearchTenantAdminViews {
    pub fn new(tx_manager: TxManager, views: Arc<dyn TenantAdminViewRepository>) -> Self {
        Self { tx_manager, views }
    }

    pub async fn execute(
        &self,
        search: &TenantViewSearch,
    ) -> Result<(Vec<TenantAdminView>, Option<TenantViewCursor>), PlatformAdminError> {
        let search = TenantViewSearch {
            limit: search.limit.clamp(1, MAX_PAGE_LIMIT),
            ..search.clone()
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_admin_search", "platform-admin")
            .await
            .map_err(storage)?;
        let page = self.views.search(&mut scope, &search).await?;
        scope.commit().await.map_err(storage)?;
        Ok(page)
    }
}

pub struct GetTenantAdminView {
    tx_manager: TxManager,
    views: Arc<dyn TenantAdminViewRepository>,
}

impl GetTenantAdminView {
    pub fn new(tx_manager: TxManager, views: Arc<dyn TenantAdminViewRepository>) -> Self {
        Self { tx_manager, views }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<TenantAdminView>, PlatformAdminError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_admin_get", "platform-admin")
            .await
            .map_err(storage)?;
        let view = self.views.get(&mut scope, tenant_id).await?;
        scope.commit().await.map_err(storage)?;
        Ok(view)
    }
}
