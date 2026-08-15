//! Tenant lifecycle transitions: activate, suspend, resume, archive.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Deserialize;
use tracing::info;
use utoipa::ToSchema;

use crate::application::get_tenant::TenantView;
use crate::domain::tenant::TenantId;
use crate::domain::values::SuspensionReason;
use crate::domain::{Clock, EventPublisher, TenancyError, TenantRepository};

use super::storage;

const ACTOR: &str = "platform-admin";

/// Request body for suspending a tenant.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SuspendTenantRequest {
    /// Machine-readable reason: billing | abuse | legal | operational.
    #[schema(example = "billing", value_type = String)]
    pub reason: String,
}

/// The transition to apply to a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    Activate,
    Suspend,
    Resume,
    Archive,
}

impl LifecycleAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Suspend => "suspend",
            Self::Resume => "resume",
            Self::Archive => "archive",
        }
    }
}

/// Shared engine behind the four lifecycle use cases.
pub struct TenantLifecycle {
    tx_manager: TxManager,
    tenants: Arc<dyn TenantRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl TenantLifecycle {
    pub fn new(
        tx_manager: TxManager,
        tenants: Arc<dyn TenantRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            tenants,
            events,
            clock,
        }
    }

    pub async fn execute(
        &self,
        id: TenantId,
        action: LifecycleAction,
        reason: Option<SuspensionReason>,
    ) -> Result<TenantView, TenancyError> {
        let now = self.clock.now();
        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_lifecycle", ACTOR)
            .await
            .map_err(storage)?;

        let mut tenant = self
            .tenants
            .find_by_id(&mut scope, id)
            .await?
            .ok_or(TenancyError::TenantNotFound { id: id.to_string() })?;

        let event = match action {
            LifecycleAction::Activate => tenant.activate(now)?,
            LifecycleAction::Suspend => tenant.suspend(
                reason.ok_or(TenancyError::InvalidSuspensionReason(
                    "suspension requires a reason".to_string(),
                ))?,
                now,
            )?,
            LifecycleAction::Resume => tenant.resume(now)?,
            LifecycleAction::Archive => tenant.archive(now)?,
        };

        self.tenants.save(&mut scope, &tenant).await?;
        self.events.publish(&mut scope, &[event]).await?;
        scope.commit().await.map_err(storage)?;

        info!(tenant_id = %tenant.id(), action = action.as_str(), "tenant lifecycle transition");
        Ok(TenantView::from(&tenant))
    }
}

macro_rules! lifecycle_use_case {
    ($name:ident, $action:expr, $doc:expr) => {
        #[doc = $doc]
        pub struct $name {
            inner: TenantLifecycle,
        }

        impl $name {
            pub fn new(
                tx_manager: TxManager,
                tenants: Arc<dyn TenantRepository>,
                events: Arc<dyn EventPublisher>,
                clock: Arc<dyn Clock>,
            ) -> Self {
                Self {
                    inner: TenantLifecycle::new(tx_manager, tenants, events, clock),
                }
            }

            pub async fn execute(&self, id: TenantId) -> Result<TenantView, TenancyError> {
                self.inner.execute(id, $action, None).await
            }
        }
    };
}

lifecycle_use_case!(
    ActivateTenant,
    LifecycleAction::Activate,
    "Provisioning → Active."
);
lifecycle_use_case!(ResumeTenant, LifecycleAction::Resume, "Suspended → Active.");
lifecycle_use_case!(
    ArchiveTenant,
    LifecycleAction::Archive,
    "Active|Suspended → Archived."
);

/// Active → Suspended (requires a machine-readable reason).
pub struct SuspendTenant {
    inner: TenantLifecycle,
}

impl SuspendTenant {
    pub fn new(
        tx_manager: TxManager,
        tenants: Arc<dyn TenantRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: TenantLifecycle::new(tx_manager, tenants, events, clock),
        }
    }

    pub async fn execute(
        &self,
        id: TenantId,
        reason: SuspensionReason,
    ) -> Result<TenantView, TenancyError> {
        self.inner
            .execute(id, LifecycleAction::Suspend, Some(reason))
            .await
    }
}
