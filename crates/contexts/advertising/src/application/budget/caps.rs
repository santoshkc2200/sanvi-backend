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
    /// Optimistic-locking guard from the `If-Match` header: the version the
    /// caller read. `None` is the legacy blind write. A mismatch is a 409 —
    /// the SQL predicate enforces it atomically; the application pre-check
    /// below only fails fast with the same error.
    #[serde(default)]
    pub expected_version: Option<i64>,
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

    #[tracing::instrument(name = "advertising.get_budget_caps", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        limit: Option<u32>,
        after_id: Option<i64>,
    ) -> Result<Vec<BudgetCap>, AdvertisingError> {
        let limit = sanvi_kernel::pagination::clamp_page_size(limit, 200, 200);
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let result = self
            .caps
            .list_by_tenant_page(&mut scope, tenant_id, limit, after_id)
            .await?;
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

    #[tracing::instrument(name = "advertising.get_campaign_budget_cap", skip_all, fields(tenant_id = %tenant_id))]
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

    #[tracing::instrument(name = "advertising.put_budget_cap", skip_all, fields(tenant_id = %tenant_id))]
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

        // Check spend to date for this scope and period. The details (not just
        // the total) matter: when spend exists in a currency no rate covers,
        // the total is incomplete and any cap written against it would be
        // unenforceable. Refuse loudly here — at save and dry-run time —
        // rather than accepting a cap the evaluator can only skip.
        let spend_details = self
            .status_query
            .calculate_spend_details(
                tenant_id,
                campaign_id,
                cmd.period,
                &cmd.amount.currency,
                cmd.declared_fx_basis.as_deref(),
                cmd.fx_rate_date,
            )
            .await?;
        if !spend_details.unconvertible_currencies.is_empty() {
            // Two different problems land in this set: cross-currency rows
            // with no declared basis are excluded before any rate is
            // consulted (a request mistake — waiting never fixes it), while
            // a declared basis can genuinely lack rate coverage (a data gap
            // — waiting can fix it). The messages must not conflate them.
            return Err(AdvertisingError::InvalidInput(
                if cmd.declared_fx_basis.is_none() {
                    format!(
                        "spend exists in currencies the cap currency {} cannot \
                         cover ({}): declare an FX basis so cross-currency \
                         spend converts, or use a cap currency matching the \
                         spend; without a basis those rows are excluded and \
                         the cap cannot be enforced",
                        cmd.amount.currency,
                        spend_details.unconvertible_currencies.join(", "),
                    )
                } else {
                    format!(
                        "budget cap currency {} cannot cover spend in {}: no FX rate is \
                         available, so this cap cannot be enforced; use a cap currency \
                         matching the spend or wait for FX coverage",
                        cmd.amount.currency,
                        spend_details.unconvertible_currencies.join(", "),
                    )
                },
            ));
        }
        // A pinned basis date is persisted verbatim on the cap and must
        // resolve to its own snapshot for the cap ever to be enforceable.
        // The spend check above only sees *current* rows: an empty or
        // same-currency scope would happily save a weekend, future, or
        // out-of-retention pin that silently disables enforcement the
        // moment foreign-currency spend appears. Validate the pin against
        // the provider directly, independent of rows.
        if let (Some(_basis), Some(pinned)) = (cmd.declared_fx_basis.as_deref(), cmd.fx_rate_date)
            && !self.status_query.has_fx_snapshot(pinned).await?
        {
            return Err(AdvertisingError::InvalidInput(format!(
                "pinned FX basis date {pinned} has no snapshot: the feed does \
                 not carry that day, so a cap pinned to it could never be \
                 enforced; pin a day the feed published",
            )));
        }
        let current_spend = spend_details.total;

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

        // A cap names a campaign inside this tenant: a foreign id must
        // answer not-found here, not surface as a foreign-key storage
        // error from the upsert below.
        if let Some(cid) = campaign_id {
            let exists = self
                .caps
                .campaign_exists(&mut scope, tenant_id, cid)
                .await?;
            if !exists {
                return Err(AdvertisingError::CampaignNotFound);
            }
        }

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

        // Optimistic locking is mandatory for updates, not advisory: an
        // existing cap written without the version its editor read is a blind
        // overwrite and is rejected outright. Only creates (no existing row)
        // may omit the precondition. The SQL predicate in `upsert_cap`
        // remains the atomic guard for races past this pre-check.
        if existing.is_some() && cmd.expected_version.is_none() {
            return Err(AdvertisingError::CapVersionRequired);
        }
        // Fail fast on a stale write before doing any more work. The SQL
        // predicate in `upsert_cap` is the atomic guard; this pre-check only
        // avoids a wasted write and reads with the same error.
        if let Some(expected) = cmd.expected_version {
            let current = existing.as_ref().map(|c| c.version);
            if current != Some(expected) {
                return Err(AdvertisingError::StaleRevision);
            }
        }

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

        let saved = self
            .caps
            .upsert_cap(&mut scope, &cap, cmd.expected_version)
            .await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        // Evaluate every applied cap change immediately: an 80% crossing or a
        // breached notification-only cap must alert now, not wait for the
        // scheduler's next tick. Errors are logged, never swallowed silently —
        // the sweep will retry on its own schedule.
        if let Err(e) = self.evaluator.evaluate_single_cap(tenant_id, &saved).await {
            tracing::warn!(
                %tenant_id,
                cap_id = %saved.id,
                error = %e,
                "Immediate post-save budget evaluation failed; scheduled sweep will retry"
            );
        }

        Ok(PutBudgetCapResponse::Applied(saved))
    }
}
