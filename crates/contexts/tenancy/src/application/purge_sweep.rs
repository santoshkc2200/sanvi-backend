//! Scheduled purge sweep: archived tenants past their retention window get a
//! (idempotent) purge request emitted. Actual deletion is phase 05.

use std::sync::Arc;

use chrono::Duration;
use sanvi_persistence::TxManager;
use tracing::info;

use crate::domain::{Clock, EventPublisher, TenancyError, TenantRepository};

use super::storage;

const ACTOR: &str = "worker";

pub struct PurgeSweep {
    tx_manager: TxManager,
    tenants: Arc<dyn TenantRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    retention: Duration,
}

impl PurgeSweep {
    pub fn new(
        tx_manager: TxManager,
        tenants: Arc<dyn TenantRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        retention_days: u32,
    ) -> Self {
        Self {
            tx_manager,
            tenants,
            events,
            clock,
            retention: Duration::days(i64::from(retention_days)),
        }
    }

    /// Emit `TenantPurgeRequested` for every eligible archived tenant.
    /// Returns how many purge requests were emitted.
    #[tracing::instrument(name = "tenancy.purge_sweep", skip_all)]
    pub async fn execute(&self, limit: u32) -> Result<u64, TenancyError> {
        let now = self.clock.now();
        let before = now - self.retention;

        let mut scope = self
            .tx_manager
            .begin_unscoped("purge_sweep", ACTOR)
            .await
            .map_err(storage)?;

        let due = self
            .tenants
            .due_for_purge_request(&mut scope, before, limit)
            .await?;

        let mut requested = 0u64;
        for mut tenant in due {
            let Some(event) = tenant.request_purge(now)? else {
                continue;
            };
            self.tenants.save(&mut scope, &tenant).await?;
            self.events.publish(&mut scope, &[event]).await?;
            requested += 1;
        }

        scope.commit().await.map_err(storage)?;
        if requested > 0 {
            info!(requested, "purge sweep emitted purge requests");
        }
        Ok(requested)
    }
}
