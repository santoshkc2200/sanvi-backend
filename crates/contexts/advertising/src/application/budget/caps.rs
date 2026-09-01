//! Budget cap management: get, put (with versioning, spend-check confirmation, and dry run).

use std::sync::Arc;

use chrono::{NaiveDate, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use utoipa::ToSchema;

use crate::domain::model::{BudgetCap, BudgetPeriod, CampaignId, Money};
use crate::domain::{AdvertisingError, BudgetCapRepository, IdGenerator};

#[derive(Debug, Clone, serde::Deserialize, ToSchema)]
pub struct PutBudgetCapCommand {
    pub period: BudgetPeriod,
    pub amount: Money,
    pub declared_fx_basis: Option<String>,
    pub fx_rate_date: Option<NaiveDate>,
    #[serde(default)]
    pub auto_pause: bool,
    #[serde(default)]
    pub auto_resume_on_rollover: bool,
    #[serde(default)]
    pub confirm_below_current_spend: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct DryRunEvaluationResult {
    pub scope: String,
    pub period: BudgetPeriod,
    pub cap_amount: Money,
    pub current_spend: Money,
    pub percentage: f64,
    pub would_breach_80: bool,
    pub would_breach_100: bool,
    pub would_auto_pause: bool,
    pub condition: String,
}

#[derive(Debug, Clone, serde::Serialize, ToSchema)]
#[serde(untagged)]
pub enum PutBudgetCapResponse {
    Applied(BudgetCap),
    DryRun(DryRunEvaluationResult),
}

pub struct GetBudgetCaps {
    tx: TxManager,
    caps: Arc<dyn BudgetCapRepository>,
}

impl GetBudgetCaps {
    pub fn new(tx: TxManager, caps: Arc<dyn BudgetCapRepository>) -> Self {
        Self { tx, caps }
    }

    pub async fn execute(&self, tenant_id: TenantId) -> Result<Vec<BudgetCap>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let result = self.caps.list_by_tenant(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(result)
    }
}

pub struct GetCampaignBudgetCap {
    tx: TxManager,
    caps: Arc<dyn BudgetCapRepository>,
}

impl GetCampaignBudgetCap {
    pub fn new(tx: TxManager, caps: Arc<dyn BudgetCapRepository>) -> Self {
        Self { tx, caps }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        period: BudgetPeriod,
    ) -> Result<BudgetCap, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let cap = self
            .caps
            .get_campaign_cap(&mut scope, tenant_id, campaign_id, period)
            .await?
            .ok_or(AdvertisingError::BudgetCapNotFound)?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(cap)
    }
}

pub struct PutBudgetCap {
    tx: TxManager,
    caps: Arc<dyn BudgetCapRepository>,
    ids: Arc<dyn IdGenerator>,
    status_query: Arc<super::status::GetSpendStatus>,
    evaluator: Arc<super::evaluator::ThresholdEvaluator>,
}

impl PutBudgetCap {
    pub fn new(
        tx: TxManager,
        caps: Arc<dyn BudgetCapRepository>,
        ids: Arc<dyn IdGenerator>,
        status_query: Arc<super::status::GetSpendStatus>,
        evaluator: Arc<super::evaluator::ThresholdEvaluator>,
    ) -> Self {
        Self {
            tx,
            caps,
            ids,
            status_query,
            evaluator,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: Option<CampaignId>,
        cmd: PutBudgetCapCommand,
    ) -> Result<PutBudgetCapResponse, AdvertisingError> {
        if cmd.amount.amount_minor <= 0 {
            return Err(AdvertisingError::InvalidInput(
                "budget cap amount must be strictly positive".to_string(),
            ));
        }

        // Check spend to date for this scope and period
        let current_spend = self
            .status_query
            .calculate_spend_to_date(
                tenant_id,
                campaign_id,
                cmd.period,
                &cmd.amount.currency,
                cmd.declared_fx_basis.as_deref(),
                cmd.fx_rate_date,
            )
            .await?;

        if cmd.dry_run {
            let percentage =
                (current_spend.amount_minor as f64 / cmd.amount.amount_minor as f64) * 100.0;
            let would_breach_80 = percentage >= 80.0;
            let would_breach_100 = percentage >= 100.0;
            let would_auto_pause = cmd.auto_pause && would_breach_100;
            let condition = if would_breach_100 {
                "breach"
            } else if would_breach_80 {
                "warning_80"
            } else {
                "ok"
            };

            let scope_str = match campaign_id {
                Some(cid) => format!("campaign:{cid}"),
                None => "tenant".to_string(),
            };

            return Ok(PutBudgetCapResponse::DryRun(DryRunEvaluationResult {
                scope: scope_str,
                period: cmd.period,
                cap_amount: cmd.amount,
                current_spend,
                percentage,
                would_breach_80,
                would_breach_100,
                would_auto_pause,
                condition: condition.to_string(),
            }));
        }

        if cmd.amount.amount_minor < current_spend.amount_minor && !cmd.confirm_below_current_spend
        {
            return Err(AdvertisingError::CapBelowCurrentSpend);
        }

        let now = Utc::now();
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let existing = match campaign_id {
            Some(cid) => {
                self.caps
                    .get_campaign_cap(&mut scope, tenant_id, cid, cmd.period)
                    .await?
            }
            None => {
                self.caps
                    .get_tenant_cap(&mut scope, tenant_id, cmd.period)
                    .await?
            }
        };

        let cap_id = match &existing {
            Some(c) => c.id,
            None => self.ids.next_budget_cap_id()?,
        };

        let version = match &existing {
            Some(c) => c.version + 1,
            None => 1,
        };

        let cap = BudgetCap {
            id: cap_id,
            tenant_id,
            campaign_id,
            period: cmd.period,
            amount: cmd.amount.clone(),
            effective_currency: cmd.amount.currency.clone(),
            declared_fx_basis: cmd.declared_fx_basis,
            fx_rate_date: cmd.fx_rate_date,
            auto_pause: cmd.auto_pause,
            auto_resume_on_rollover: cmd.auto_resume_on_rollover,
            version,
            created_at: existing.as_ref().map(|c| c.created_at).unwrap_or(now),
            updated_at: now,
        };

        let saved = self.caps.upsert_cap(&mut scope, &cap).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        // If auto-pause is enabled and spend exceeds cap, evaluate threshold immediately
        if saved.auto_pause && current_spend.amount_minor >= saved.amount.amount_minor {
            let _ = self.evaluator.evaluate_single_cap(tenant_id, &saved).await;
        }

        Ok(PutBudgetCapResponse::Applied(saved))
    }
}
