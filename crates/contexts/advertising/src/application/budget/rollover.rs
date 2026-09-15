//! Budget period rollover sweep (TASK-017, slice 10.8).
//!
//! # Auto-resume: intentionally absent
//!
//! FR-1013 requires that budget guardrails **never auto-resume** a campaign.
//! The `auto_resume_on_rollover` field on `BudgetCap` is retained in the schema
//! for backward compatibility but its functional effect has been removed.
//!
//! What remains is the period boundary itself. Spend is always recomputed from
//! the metrics rows inside the current period (see `GetSpendStatus`), so there
//! is no stored counter to zero; crossing a boundary simply means a new
//! `period_start`, and therefore a fresh set of threshold alerts. This sweep
//! records that crossing once per period and leaves paused campaigns paused —
//! resuming one requires a human operator or an explicit API call.
//!
//! # Boundary guard
//!
//! The scheduler fires this job on a fixed interval (hourly by default), which
//! is far more often than a period actually rolls over. `sweep_tenant` compares
//! the current `period_start` against the last one it processed and does nothing
//! when they match, so a tick that crosses no boundary is a genuine no-op.
//!
//! That marker is in-process: a worker restart forgets it and the next tick
//! re-records the current period. Since the sweep's only remaining effect is a
//! log line, re-recording is harmless. Making it durable would need a
//! `last_rolled_over_at` column on the cap and is deliberately not done here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{NaiveDate, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::model::BudgetPeriod;
use crate::domain::{AdvertisingError, BudgetCapRepository, TenantTimezoneSource};

pub struct BudgetRolloverSweep {
    tx: TxManager,
    caps: Arc<dyn BudgetCapRepository>,
    timezones: Arc<dyn TenantTimezoneSource>,
    /// Last `period_start` this process recorded per (tenant, period). See the
    /// module docs for why this is in-process rather than persisted.
    last_processed: Mutex<HashMap<(TenantId, BudgetPeriod), NaiveDate>>,
}

impl BudgetRolloverSweep {
    pub fn new(
        tx: TxManager,
        caps: Arc<dyn BudgetCapRepository>,
        timezones: Arc<dyn TenantTimezoneSource>,
    ) -> Self {
        Self {
            tx,
            caps,
            timezones,
            last_processed: Mutex::new(HashMap::new()),
        }
    }

    /// Record the period boundary for `tenant_id`, if one has actually been
    /// crossed since this process last looked.
    ///
    /// Returns `true` when a boundary was recorded, `false` when the tick landed
    /// inside the period already processed. Campaigns are never resumed
    /// (FR-1013).
    pub async fn sweep_tenant(
        &self,
        tenant_id: TenantId,
        period: BudgetPeriod,
    ) -> Result<bool, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        // The boundary is "today" in the tenant's timezone — the same window
        // spend is summed in — not UTC. A UTC boundary would record a
        // rollover hours before (or after) the spend window actually turns
        // over for a non-UTC tenant.
        let tz_name = self.timezones.timezone(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let tz = super::status::parse_tenant_timezone(&tz_name);
        let period_start = period.period_start(Utc::now().with_timezone(&tz).date_naive());

        // Claim the boundary before doing any work, so a concurrent tick cannot
        // process the same period twice. The guard is dropped before any await.
        {
            let mut seen = self
                .last_processed
                .lock()
                .map_err(|_| AdvertisingError::Storage("rollover state poisoned".into()))?;
            match seen.get(&(tenant_id, period)) {
                Some(last) if *last == period_start => return Ok(false),
                _ => seen.insert((tenant_id, period), period_start),
            };
        }

        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let caps_list = self.caps.list_by_tenant(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let cap_count = caps_list.iter().filter(|c| c.period == period).count();
        if cap_count > 0 {
            tracing::info!(
                %tenant_id,
                period = period.as_str(),
                %period_start,
                cap_count,
                "Budget rollover: new period boundary recorded; \
                 no campaigns auto-resumed (FR-1013)"
            );
        }

        Ok(true)
    }

    /// Sweep every tenant holding an active cap for `period`.
    ///
    /// Returns the number of tenants for which a boundary was newly recorded.
    /// The cap listing runs in its own short transaction, committed before the
    /// per-tenant work begins, so no platform transaction is held across the
    /// sweep.
    pub async fn sweep_all(&self, period: BudgetPeriod) -> Result<usize, AdvertisingError> {
        let tenants = {
            let mut scope = self
                .tx
                .begin_unscoped("advertising_budget_rollover", "system")
                .await
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
            let caps = self.caps.list_all_active_caps(&mut scope).await?;
            scope
                .commit()
                .await
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

            let mut tenants: Vec<TenantId> = caps.into_iter().map(|c| c.tenant_id).collect();
            tenants.sort_unstable();
            tenants.dedup();
            tenants
        };

        let mut rolled = 0;
        for tenant_id in tenants {
            match self.sweep_tenant(tenant_id, period).await {
                Ok(true) => rolled += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    %tenant_id,
                    period = period.as_str(),
                    error = %e,
                    "Budget rollover sweep failed for tenant; continuing"
                ),
            }
        }

        Ok(rolled)
    }
}
