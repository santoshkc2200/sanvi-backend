//! Default notifier implementation for advertising budget guardrails.

use async_trait::async_trait;
use sanvi_kernel::TenantId;

use crate::domain::model::{BudgetPeriod, Money};
use crate::domain::{AdvertisingError, BudgetNotifier};

#[derive(Debug, Clone, Copy, Default)]
pub struct LoggingBudgetNotifier;

impl LoggingBudgetNotifier {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BudgetNotifier for LoggingBudgetNotifier {
    async fn notify_threshold_80(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        spend: &Money,
        cap_amount: &Money,
        period: BudgetPeriod,
    ) -> Result<(), AdvertisingError> {
        tracing::info!(
            %tenant_id,
            scope_name,
            spend = %spend.amount_minor,
            cap = %cap_amount.amount_minor,
            period = period.as_str(),
            "Budget 80% threshold reached"
        );
        Ok(())
    }

    async fn notify_cap_breached(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        spend: &Money,
        cap_amount: &Money,
        period: BudgetPeriod,
        action_taken: &str,
    ) -> Result<(), AdvertisingError> {
        tracing::warn!(
            %tenant_id,
            scope_name,
            spend = %spend.amount_minor,
            cap = %cap_amount.amount_minor,
            period = period.as_str(),
            action_taken,
            "Budget cap breached"
        );
        Ok(())
    }

    async fn notify_provisional_warning(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        spend: &Money,
        cap_amount: &Money,
        lower_bound: &Money,
        period: BudgetPeriod,
    ) -> Result<(), AdvertisingError> {
        tracing::warn!(
            %tenant_id,
            scope_name,
            spend = %spend.amount_minor,
            cap = %cap_amount.amount_minor,
            lower_bound = %lower_bound.amount_minor,
            period = period.as_str(),
            "Provisional budget warning"
        );
        Ok(())
    }

    async fn notify_stale_warning(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        lag_hours: f64,
    ) -> Result<(), AdvertisingError> {
        tracing::warn!(
            %tenant_id,
            scope_name,
            lag_hours,
            "Advertising data stale warning"
        );
        Ok(())
    }

    async fn notify_spend_anomaly(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        current_spend: &Money,
        trailing_mean: &Money,
        sigma_multiple: f64,
    ) -> Result<(), AdvertisingError> {
        tracing::warn!(
            %tenant_id,
            scope_name,
            current_spend = %current_spend.amount_minor,
            trailing_mean = %trailing_mean.amount_minor,
            sigma_multiple,
            "Advertising spend anomaly detected"
        );
        Ok(())
    }

    async fn notify_guardrails_rollback(
        &self,
        tenant_id: TenantId,
    ) -> Result<(), AdvertisingError> {
        tracing::warn!(
            %tenant_id,
            "Advertising budget guardrails rollback notice"
        );
        Ok(())
    }
}
