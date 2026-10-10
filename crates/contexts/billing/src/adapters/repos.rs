//! The sqlx billing repositories (`billing.plans`, `billing.plan_prices`,
//! `billing.plan_entitlements`, `billing.subscriptions`, `billing.invoices`,
//! `billing.webhook_events`). Tenant-scoped reads run under `ScopedTx`
//! (RLS-filtered by construction); writes and cross-tenant maintenance run
//! under the audited `PlatformScope`.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::{PlatformScope, ScopedTx};
use sqlx::FromRow;
use uuid::Uuid;

use crate::domain::{
    BillingError, CheckoutState, CheckoutStateRepository, CollectionState, EntitlementGrant,
    InvoiceId, InvoiceRef, InvoiceStatus, Period, Plan, PlanId, PlanPrice, PlanPriceRepository,
    PlanRepository, PlanTier, PlanVisibility, PriceId, PriceInterval, Subscription, SubscriptionId,
    SubscriptionRepository, SubscriptionSource, SubscriptionStatus, WebhookEventRecord,
    WebhookEventRepository,
};

fn map_db(err: sqlx::Error) -> BillingError {
    BillingError::Storage(err.to_string())
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct PlanRow {
    id: i64,
    key: String,
    name: String,
    tier: String,
    stripe_product_id: String,
    visibility: String,
    sort_order: i32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn to_plan(row: PlanRow) -> Result<Plan, BillingError> {
    Ok(Plan::from_parts(
        PlanId::try_from(row.id).map_err(|err| BillingError::Storage(err.to_string()))?,
        row.key,
        row.name,
        PlanTier::parse(&row.tier)?,
        row.stripe_product_id,
        PlanVisibility::parse(&row.visibility)?,
        row.sort_order,
        row.created_at,
        row.updated_at,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxPlanRepository;

impl SqlxPlanRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PlanRepository for SqlxPlanRepository {
    async fn save(&self, scope: &mut PlatformScope<'_>, plan: &Plan) -> Result<(), BillingError> {
        sqlx::query(
            "INSERT INTO billing.plans
               (id, key, name, tier, stripe_product_id, visibility, sort_order, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE SET
               name = EXCLUDED.name,
               tier = EXCLUDED.tier,
               stripe_product_id = EXCLUDED.stripe_product_id,
               visibility = EXCLUDED.visibility,
               sort_order = EXCLUDED.sort_order,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(plan.id().as_i64())
        .bind(plan.key())
        .bind(plan.name())
        .bind(plan.tier().as_str())
        .bind(plan.stripe_product_id())
        .bind(plan.visibility().as_str())
        .bind(plan.sort_order())
        .bind(plan.created_at())
        .bind(plan.updated_at())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: PlanId,
    ) -> Result<Option<Plan>, BillingError> {
        let row: Option<PlanRow> = sqlx::query_as(
            "SELECT id, key, name, tier, stripe_product_id, visibility, sort_order,
                    created_at, updated_at
             FROM billing.plans WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_plan).transpose()
    }

    async fn find_by_key(
        &self,
        scope: &mut PlatformScope<'_>,
        key: &str,
    ) -> Result<Option<Plan>, BillingError> {
        let row: Option<PlanRow> = sqlx::query_as(
            "SELECT id, key, name, tier, stripe_product_id, visibility, sort_order,
                    created_at, updated_at
             FROM billing.plans WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_plan).transpose()
    }

    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        public_only: bool,
    ) -> Result<Vec<Plan>, BillingError> {
        let sql = if public_only {
            "SELECT id, key, name, tier, stripe_product_id, visibility, sort_order,
                    created_at, updated_at
             FROM billing.plans WHERE visibility = 'public' ORDER BY sort_order, key"
        } else {
            "SELECT id, key, name, tier, stripe_product_id, visibility, sort_order,
                    created_at, updated_at
             FROM billing.plans ORDER BY sort_order, key"
        };
        let rows: Vec<PlanRow> = sqlx::query_as(sql)
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        rows.into_iter().map(to_plan).collect()
    }
}

// ---------------------------------------------------------------------------
// Prices
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct PriceRow {
    id: i64,
    plan_id: i64,
    stripe_price_id: String,
    currency: String,
    interval: String,
    unit_amount_minor: i64,
    trial_days: Option<i32>,
    active: bool,
    created_at: DateTime<Utc>,
}

fn to_price(row: PriceRow) -> Result<PlanPrice, BillingError> {
    Ok(PlanPrice::from_parts(
        PriceId::try_from(row.id).map_err(|err| BillingError::Storage(err.to_string()))?,
        PlanId::try_from(row.plan_id).map_err(|err| BillingError::Storage(err.to_string()))?,
        row.stripe_price_id,
        row.currency,
        PriceInterval::parse(&row.interval)?,
        row.unit_amount_minor,
        row.trial_days.map(|d| d as u16),
        row.active,
        row.created_at,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxPlanPriceRepository;

impl SqlxPlanPriceRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PlanPriceRepository for SqlxPlanPriceRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        price: &PlanPrice,
    ) -> Result<(), BillingError> {
        sqlx::query(
            "INSERT INTO billing.plan_prices
               (id, plan_id, stripe_price_id, currency, interval, unit_amount_minor, trial_days, active, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE SET
               currency = EXCLUDED.currency,
               interval = EXCLUDED.interval,
               unit_amount_minor = EXCLUDED.unit_amount_minor,
               trial_days = EXCLUDED.trial_days,
               active = EXCLUDED.active",
        )
        .bind(price.id().as_i64())
        .bind(price.plan_id().as_i64())
        .bind(price.stripe_price_id())
        .bind(price.currency())
        .bind(price.interval().as_str())
        .bind(price.unit_amount_minor())
        .bind(price.trial_days().map(i32::from))
        .bind(price.active())
        .bind(price.created_at())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: PriceId,
    ) -> Result<Option<PlanPrice>, BillingError> {
        let row: Option<PriceRow> = sqlx::query_as(
            "SELECT id, plan_id, stripe_price_id, currency, interval, unit_amount_minor,
                    trial_days, active, created_at
             FROM billing.plan_prices WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_price).transpose()
    }

    async fn find_by_stripe_price_id(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_price_id: &str,
    ) -> Result<Option<PlanPrice>, BillingError> {
        let row: Option<PriceRow> = sqlx::query_as(
            "SELECT id, plan_id, stripe_price_id, currency, interval, unit_amount_minor,
                    trial_days, active, created_at
             FROM billing.plan_prices WHERE stripe_price_id = $1",
        )
        .bind(stripe_price_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_price).transpose()
    }

    async fn list_active_for_plan(
        &self,
        scope: &mut PlatformScope<'_>,
        plan_id: PlanId,
    ) -> Result<Vec<PlanPrice>, BillingError> {
        let rows: Vec<PriceRow> = sqlx::query_as(
            "SELECT id, plan_id, stripe_price_id, currency, interval, unit_amount_minor,
                    trial_days, active, created_at
             FROM billing.plan_prices WHERE plan_id = $1 AND active ORDER BY unit_amount_minor",
        )
        .bind(plan_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_price).collect()
    }

    async fn list_active_for_plans(
        &self,
        scope: &mut PlatformScope<'_>,
        plan_ids: &[PlanId],
    ) -> Result<BTreeMap<PlanId, Vec<PlanPrice>>, BillingError> {
        let mut grouped: BTreeMap<PlanId, Vec<PlanPrice>> = BTreeMap::new();
        if plan_ids.is_empty() {
            return Ok(grouped);
        }
        let ids: Vec<i64> = plan_ids.iter().map(|id| id.as_i64()).collect();
        let rows: Vec<PriceRow> = sqlx::query_as(
            "SELECT id, plan_id, stripe_price_id, currency, interval, unit_amount_minor,
                    trial_days, active, created_at
             FROM billing.plan_prices
             WHERE active AND plan_id = ANY($1)
             ORDER BY plan_id, unit_amount_minor",
        )
        .bind(&ids)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        for row in rows {
            let price = to_price(row)?;
            grouped.entry(price.plan_id()).or_default().push(price);
        }
        Ok(grouped)
    }
}

// ---------------------------------------------------------------------------
// Plan entitlements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct GrantRow {
    feature_key: String,
    enabled: bool,
    limit: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxPlanEntitlementRepository;

impl SqlxPlanEntitlementRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl crate::domain::PlanEntitlementRepository for SqlxPlanEntitlementRepository {
    async fn replace_for_plan(
        &self,
        scope: &mut PlatformScope<'_>,
        plan_id: PlanId,
        grants: &[EntitlementGrant],
    ) -> Result<(), BillingError> {
        sqlx::query("DELETE FROM billing.plan_entitlements WHERE plan_id = $1")
            .bind(plan_id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        for grant in grants {
            sqlx::query(
                "INSERT INTO billing.plan_entitlements (plan_id, feature_key, enabled, \"limit\")
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(plan_id.as_i64())
            .bind(&grant.feature_key)
            .bind(grant.enabled)
            .bind(grant.limit)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        }
        Ok(())
    }

    async fn list_for_plan(
        &self,
        scope: &mut PlatformScope<'_>,
        plan_id: PlanId,
    ) -> Result<Vec<EntitlementGrant>, BillingError> {
        let rows: Vec<GrantRow> = sqlx::query_as(
            "SELECT feature_key, enabled, \"limit\"
             FROM billing.plan_entitlements WHERE plan_id = $1 ORDER BY feature_key",
        )
        .bind(plan_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(rows
            .into_iter()
            .map(|row| EntitlementGrant {
                feature_key: row.feature_key,
                enabled: row.enabled,
                limit: row.limit,
            })
            .collect())
    }

    async fn list_for_plans(
        &self,
        scope: &mut PlatformScope<'_>,
        plan_ids: &[PlanId],
    ) -> Result<BTreeMap<PlanId, Vec<EntitlementGrant>>, BillingError> {
        let mut grouped: BTreeMap<PlanId, Vec<EntitlementGrant>> = BTreeMap::new();
        if plan_ids.is_empty() {
            return Ok(grouped);
        }
        let ids: Vec<i64> = plan_ids.iter().map(|id| id.as_i64()).collect();
        let rows: Vec<(i64, String, bool, Option<i64>)> = sqlx::query_as(
            "SELECT plan_id, feature_key, enabled, \"limit\"
             FROM billing.plan_entitlements
             WHERE plan_id = ANY($1)
             ORDER BY plan_id, feature_key",
        )
        .bind(&ids)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        for (plan_id, feature_key, enabled, limit) in rows {
            let plan_id =
                PlanId::try_from(plan_id).map_err(|err| BillingError::Storage(err.to_string()))?;
            grouped.entry(plan_id).or_default().push(EntitlementGrant {
                feature_key,
                enabled,
                limit,
            });
        }
        Ok(grouped)
    }
}

// ---------------------------------------------------------------------------
// Durable Checkout state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct CheckoutStateRow {
    tenant_id: i64,
    price_id: i64,
    command_id: Uuid,
    stripe_customer_id: Option<String>,
    stripe_session_id: Option<String>,
    session_url: Option<String>,
    success_url: String,
    cancel_url: String,
    integration_identifier_suffix: String,
    expires_at: DateTime<Utc>,
}

fn to_checkout_state(row: CheckoutStateRow) -> Result<CheckoutState, BillingError> {
    Ok(CheckoutState {
        tenant_id: TenantId::try_from(row.tenant_id)
            .map_err(|err| BillingError::Storage(err.to_string()))?,
        price_id: PriceId::try_from(row.price_id)
            .map_err(|err| BillingError::Storage(err.to_string()))?,
        command_id: row.command_id,
        stripe_customer_id: row.stripe_customer_id,
        stripe_session_id: row.stripe_session_id,
        session_url: row.session_url,
        success_url: row.success_url,
        cancel_url: row.cancel_url,
        integration_identifier_suffix: row.integration_identifier_suffix,
        expires_at: row.expires_at,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxCheckoutStateRepository;

impl SqlxCheckoutStateRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl CheckoutStateRepository for SqlxCheckoutStateRepository {
    async fn claim(
        &self,
        scope: &mut PlatformScope<'_>,
        proposed: &CheckoutState,
        now: DateTime<Utc>,
        force_new: bool,
    ) -> Result<CheckoutState, BillingError> {
        let row: Option<CheckoutStateRow> = sqlx::query_as(
            "INSERT INTO billing.checkout_states
               (tenant_id, price_id, command_id, stripe_customer_id,
                stripe_session_id, session_url, success_url, cancel_url,
                integration_identifier_suffix, expires_at, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now(), now())
             ON CONFLICT (tenant_id) DO UPDATE SET
               price_id = EXCLUDED.price_id,
               command_id = EXCLUDED.command_id,
               stripe_customer_id = COALESCE(billing.checkout_states.stripe_customer_id,
                                             EXCLUDED.stripe_customer_id),
               stripe_session_id = NULL,
               session_url = NULL,
               success_url = EXCLUDED.success_url,
               cancel_url = EXCLUDED.cancel_url,
               integration_identifier_suffix = EXCLUDED.integration_identifier_suffix,
               expires_at = EXCLUDED.expires_at,
               updated_at = now()
             WHERE billing.checkout_states.expires_at <= $11 OR $12
             RETURNING tenant_id, price_id, command_id, stripe_customer_id,
                       stripe_session_id, session_url, success_url, cancel_url,
                       integration_identifier_suffix, expires_at",
        )
        .bind(proposed.tenant_id.as_i64())
        .bind(proposed.price_id.as_i64())
        .bind(proposed.command_id)
        .bind(&proposed.stripe_customer_id)
        .bind(&proposed.stripe_session_id)
        .bind(&proposed.session_url)
        .bind(&proposed.success_url)
        .bind(&proposed.cancel_url)
        .bind(&proposed.integration_identifier_suffix)
        .bind(proposed.expires_at)
        .bind(now)
        .bind(force_new)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;

        let row = match row {
            Some(row) => row,
            None => sqlx::query_as(
                "SELECT tenant_id, price_id, command_id, stripe_customer_id,
                        stripe_session_id, session_url, success_url, cancel_url,
                        integration_identifier_suffix, expires_at
                 FROM billing.checkout_states WHERE tenant_id = $1",
            )
            .bind(proposed.tenant_id.as_i64())
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?,
        };
        to_checkout_state(row)
    }

    async fn set_customer(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        command_id: Uuid,
        stripe_customer_id: &str,
    ) -> Result<(), BillingError> {
        let result = sqlx::query(
            "UPDATE billing.checkout_states
             SET stripe_customer_id = $3, updated_at = now()
             WHERE tenant_id = $1 AND command_id = $2",
        )
        .bind(tenant_id.as_i64())
        .bind(command_id)
        .bind(stripe_customer_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        if result.rows_affected() == 0 {
            return Err(BillingError::Conflict(
                "checkout state was replaced while creating its customer".to_string(),
            ));
        }
        Ok(())
    }

    async fn complete(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        command_id: Uuid,
        stripe_session_id: &str,
        session_url: &str,
    ) -> Result<(), BillingError> {
        let result = sqlx::query(
            "UPDATE billing.checkout_states
             SET stripe_session_id = $3, session_url = $4, updated_at = now()
             WHERE tenant_id = $1 AND command_id = $2",
        )
        .bind(tenant_id.as_i64())
        .bind(command_id)
        .bind(stripe_session_id)
        .bind(session_url)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        if result.rows_affected() == 0 {
            return Err(BillingError::Conflict(
                "checkout state was replaced while creating its session".to_string(),
            ));
        }
        Ok(())
    }

    async fn customer_for_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<String>, BillingError> {
        sqlx::query_scalar(
            "SELECT stripe_customer_id FROM billing.checkout_states WHERE tenant_id = $1",
        )
        .bind(tenant_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)
        .map(Option::flatten)
    }

    async fn anonymise(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<usize, BillingError> {
        let result = sqlx::query(
            "UPDATE billing.checkout_states
             SET stripe_customer_id = NULL, stripe_session_id = NULL,
                 session_url = NULL, updated_at = now()
             WHERE tenant_id = $1
               AND (stripe_customer_id IS NOT NULL OR stripe_session_id IS NOT NULL
                    OR session_url IS NOT NULL)",
        )
        .bind(tenant_id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(result.rows_affected() as usize)
    }
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct SubscriptionRow {
    id: i64,
    tenant_id: i64,
    plan_id: i64,
    price_id: Option<i64>,
    source: String,
    stripe_customer_id: Option<String>,
    stripe_subscription_id: Option<String>,
    status: String,
    current_period_start: Option<DateTime<Utc>>,
    current_period_end: Option<DateTime<Utc>>,
    cancel_at_period_end: bool,
    trial_end: Option<DateTime<Utc>>,
    trial_notified_at: Option<DateTime<Utc>>,
    collection_state: String,
    collection_started_at: Option<DateTime<Utc>>,
    stripe_subscription_event_at: Option<DateTime<Utc>>,
    stripe_invoice_event_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn to_subscription(row: SubscriptionRow) -> Result<Subscription, BillingError> {
    let period = match (row.current_period_start, row.current_period_end) {
        (Some(start), Some(end)) => Some(Period { start, end }),
        _ => None,
    };
    Ok(Subscription::from_parts(
        SubscriptionId::try_from(row.id).map_err(|err| BillingError::Storage(err.to_string()))?,
        TenantId::try_from(row.tenant_id).map_err(|err| BillingError::Storage(err.to_string()))?,
        PlanId::try_from(row.plan_id).map_err(|err| BillingError::Storage(err.to_string()))?,
        row.price_id
            .map(PriceId::try_from)
            .transpose()
            .map_err(|err| BillingError::Storage(err.to_string()))?,
        SubscriptionSource::parse(&row.source)?,
        row.stripe_customer_id,
        row.stripe_subscription_id,
        SubscriptionStatus::parse(&row.status)?,
        period,
        row.cancel_at_period_end,
        row.trial_end,
        row.trial_notified_at,
        CollectionState::parse(&row.collection_state)?,
        row.collection_started_at,
        row.stripe_subscription_event_at,
        row.stripe_invoice_event_at,
        row.created_at,
        row.updated_at,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxSubscriptionRepository;

impl SqlxSubscriptionRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SubscriptionRepository for SqlxSubscriptionRepository {
    async fn anonymise_stripe_ids(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<usize, BillingError> {
        let result = sqlx::query(
            "UPDATE billing.subscriptions
             SET stripe_customer_id = NULL, stripe_subscription_id = NULL, updated_at = now()
             WHERE tenant_id = $1",
        )
        .bind(tenant_id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(result.rows_affected() as usize)
    }

    async fn delete_by_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<bool, BillingError> {
        let result = sqlx::query("DELETE FROM billing.subscriptions WHERE tenant_id = $1")
            .bind(tenant_id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(map_db)?;
        Ok(result.rows_affected() > 0)
    }

    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        subscription: &Subscription,
    ) -> Result<(), BillingError> {
        let (period_start, period_end) = subscription
            .current_period()
            .map(|p| (Some(p.start), Some(p.end)))
            .unwrap_or((None, None));
        sqlx::query(
            "INSERT INTO billing.subscriptions
               (id, tenant_id, plan_id, price_id, source, stripe_customer_id,
                stripe_subscription_id, status, current_period_start, current_period_end,
                cancel_at_period_end, trial_end, trial_notified_at, collection_state,
                collection_started_at, stripe_subscription_event_at, stripe_invoice_event_at,
                created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
             ON CONFLICT (id) DO UPDATE SET
               plan_id = EXCLUDED.plan_id,
               price_id = EXCLUDED.price_id,
               source = EXCLUDED.source,
               stripe_customer_id = EXCLUDED.stripe_customer_id,
               stripe_subscription_id = EXCLUDED.stripe_subscription_id,
               status = EXCLUDED.status,
               current_period_start = EXCLUDED.current_period_start,
               current_period_end = EXCLUDED.current_period_end,
               cancel_at_period_end = EXCLUDED.cancel_at_period_end,
               trial_end = EXCLUDED.trial_end,
               trial_notified_at = EXCLUDED.trial_notified_at,
               collection_state = EXCLUDED.collection_state,
               collection_started_at = EXCLUDED.collection_started_at,
               stripe_subscription_event_at = EXCLUDED.stripe_subscription_event_at,
               stripe_invoice_event_at = EXCLUDED.stripe_invoice_event_at,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(subscription.id().as_i64())
        .bind(subscription.tenant_id().as_i64())
        .bind(subscription.plan_id().as_i64())
        .bind(subscription.price_id().map(|p| p.as_i64()))
        .bind(subscription.source().as_str())
        .bind(subscription.stripe_customer_id())
        .bind(subscription.stripe_subscription_id())
        .bind(subscription.status().as_str())
        .bind(period_start)
        .bind(period_end)
        .bind(subscription.cancel_at_period_end())
        .bind(subscription.trial_end())
        .bind(subscription.trial_notified_at())
        .bind(subscription.collection_state().as_str())
        .bind(subscription.collection_started_at())
        .bind(subscription.stripe_subscription_event_at())
        .bind(subscription.stripe_invoice_event_at())
        .bind(subscription.created_at())
        .bind(subscription.updated_at())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn find_by_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<Subscription>, BillingError> {
        let row: Option<SubscriptionRow> = sqlx::query_as("SELECT id, tenant_id, plan_id, price_id, source, stripe_customer_id, stripe_subscription_id, status, current_period_start, current_period_end, cancel_at_period_end, trial_end, trial_notified_at, collection_state, collection_started_at, stripe_subscription_event_at, stripe_invoice_event_at, created_at, updated_at FROM billing.subscriptions WHERE tenant_id = $1")
        .bind(tenant_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_subscription).transpose()
    }

    async fn find_by_tenant_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<Subscription>, BillingError> {
        let row: Option<SubscriptionRow> = sqlx::query_as("SELECT id, tenant_id, plan_id, price_id, source, stripe_customer_id, stripe_subscription_id, status, current_period_start, current_period_end, cancel_at_period_end, trial_end, trial_notified_at, collection_state, collection_started_at, stripe_subscription_event_at, stripe_invoice_event_at, created_at, updated_at FROM billing.subscriptions WHERE tenant_id = $1")
        .bind(tenant_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_subscription).transpose()
    }

    async fn find_by_stripe_subscription_id(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_subscription_id: &str,
    ) -> Result<Option<Subscription>, BillingError> {
        let row: Option<SubscriptionRow> = sqlx::query_as("SELECT id, tenant_id, plan_id, price_id, source, stripe_customer_id, stripe_subscription_id, status, current_period_start, current_period_end, cancel_at_period_end, trial_end, trial_notified_at, collection_state, collection_started_at, stripe_subscription_event_at, stripe_invoice_event_at, created_at, updated_at FROM billing.subscriptions WHERE stripe_subscription_id = $1")
        .bind(stripe_subscription_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_subscription).transpose()
    }

    async fn list_reconcilable(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<Subscription>, BillingError> {
        let rows: Vec<SubscriptionRow> = sqlx::query_as("SELECT id, tenant_id, plan_id, price_id, source, stripe_customer_id, stripe_subscription_id, status, current_period_start, current_period_end, cancel_at_period_end, trial_end, trial_notified_at, collection_state, collection_started_at, stripe_subscription_event_at, stripe_invoice_event_at, created_at, updated_at FROM billing.subscriptions WHERE source = 'stripe' AND status IN ('trialing', 'active', 'past_due', 'unpaid', 'paused') ORDER BY updated_at LIMIT $1")
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_subscription).collect()
    }

    async fn list_dunning_due(
        &self,
        scope: &mut PlatformScope<'_>,
        deadline: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Subscription>, BillingError> {
        let rows: Vec<SubscriptionRow> = sqlx::query_as("SELECT s.id, s.tenant_id, s.plan_id, s.price_id, s.source, s.stripe_customer_id, s.stripe_subscription_id, s.status, s.current_period_start, s.current_period_end, s.cancel_at_period_end, s.trial_end, s.trial_notified_at, s.collection_state, s.collection_started_at, s.stripe_subscription_event_at, s.stripe_invoice_event_at, s.created_at, s.updated_at FROM billing.subscriptions s JOIN tenancy.tenants t ON t.id = s.tenant_id WHERE (s.collection_state = 'dunning' AND s.collection_started_at <= $1) OR (s.collection_state = 'grace_expired' AND t.status = 'active') ORDER BY s.collection_started_at LIMIT $2")
        .bind(deadline)
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_subscription).collect()
    }

    async fn list_trials_ending_soon(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        within: Duration,
        limit: u32,
    ) -> Result<Vec<Subscription>, BillingError> {
        let horizon = now + within;
        let rows: Vec<SubscriptionRow> = sqlx::query_as("SELECT id, tenant_id, plan_id, price_id, source, stripe_customer_id, stripe_subscription_id, status, current_period_start, current_period_end, cancel_at_period_end, trial_end, trial_notified_at, collection_state, collection_started_at, stripe_subscription_event_at, stripe_invoice_event_at, created_at, updated_at FROM billing.subscriptions WHERE status = 'trialing' AND trial_end IS NOT NULL AND trial_end > $1 AND trial_end <= $2 AND trial_notified_at IS NULL ORDER BY trial_end LIMIT $3")
        .bind(now)
        .bind(horizon)
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_subscription).collect()
    }

    async fn count_by_status(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<(SubscriptionStatus, i64)>, BillingError> {
        #[derive(Debug, Clone, FromRow)]
        struct StatusCount {
            status: String,
            count: i64,
        }
        let rows: Vec<StatusCount> =
            sqlx::query_as("SELECT status, count(*) FROM billing.subscriptions GROUP BY status")
                .fetch_all(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(map_db)?;
        rows.into_iter()
            .map(|row| Ok((SubscriptionStatus::parse(&row.status)?, row.count)))
            .collect()
    }

    async fn dunning_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, BillingError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing.subscriptions WHERE collection_state = 'dunning'",
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(count)
    }

    async fn mrr_by_currency(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<(String, f64)>, BillingError> {
        #[derive(Debug, FromRow)]
        struct CurrencyMrr {
            currency: String,
            amount: f64,
        }

        let rows: Vec<CurrencyMrr> = sqlx::query_as(
            "SELECT p.currency,
                    SUM(CASE p.interval
                          WHEN 'year' THEN p.unit_amount_minor::double precision / 12.0
                          ELSE p.unit_amount_minor::double precision
                        END) AS amount
             FROM billing.subscriptions s
             JOIN billing.plan_prices p ON p.id = s.price_id
             WHERE s.status IN ('trialing', 'active', 'past_due')
             GROUP BY p.currency
             ORDER BY p.currency",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(rows
            .into_iter()
            .map(|row| (row.currency, row.amount))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Invoices
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct InvoiceRow {
    id: i64,
    tenant_id: i64,
    stripe_invoice_id: String,
    number: Option<String>,
    status: String,
    total_minor: i64,
    currency: String,
    hosted_url: Option<String>,
    pdf_url: Option<String>,
    period_start: Option<DateTime<Utc>>,
    period_end: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    paid_at: Option<DateTime<Utc>>,
}

fn to_invoice(row: InvoiceRow) -> Result<InvoiceRef, BillingError> {
    let period = match (row.period_start, row.period_end) {
        (Some(start), Some(end)) => Some(Period { start, end }),
        _ => None,
    };
    Ok(InvoiceRef::from_parts(
        InvoiceId::try_from(row.id).map_err(|err| BillingError::Storage(err.to_string()))?,
        TenantId::try_from(row.tenant_id).map_err(|err| BillingError::Storage(err.to_string()))?,
        row.stripe_invoice_id,
        row.number,
        InvoiceStatus::parse(&row.status)?,
        row.total_minor,
        row.currency,
        row.hosted_url,
        row.pdf_url,
        period,
        row.created_at,
        row.paid_at,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxInvoiceRepository;

impl SqlxInvoiceRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl crate::domain::InvoiceRepository for SqlxInvoiceRepository {
    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        invoice: &InvoiceRef,
        event_created: DateTime<Utc>,
    ) -> Result<bool, BillingError> {
        let (period_start, period_end) = invoice
            .period()
            .map(|p| (Some(p.start), Some(p.end)))
            .unwrap_or((None, None));
        let result = sqlx::query(
            "INSERT INTO billing.invoices
               (id, tenant_id, stripe_invoice_id, number, status, total_minor, currency,
                hosted_url, pdf_url, period_start, period_end, created_at, paid_at,
                stripe_event_created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (stripe_invoice_id) DO UPDATE SET
               number = EXCLUDED.number,
               status = EXCLUDED.status,
               total_minor = EXCLUDED.total_minor,
               currency = EXCLUDED.currency,
               hosted_url = EXCLUDED.hosted_url,
               pdf_url = EXCLUDED.pdf_url,
               period_start = EXCLUDED.period_start,
               period_end = EXCLUDED.period_end,
               paid_at = EXCLUDED.paid_at,
               stripe_event_created_at = EXCLUDED.stripe_event_created_at
             WHERE billing.invoices.stripe_event_created_at IS NULL
                OR billing.invoices.stripe_event_created_at <= EXCLUDED.stripe_event_created_at",
        )
        .bind(invoice.id().as_i64())
        .bind(invoice.tenant_id().as_i64())
        .bind(invoice.stripe_invoice_id())
        .bind(invoice.number())
        .bind(invoice.status().as_str())
        .bind(invoice.total_minor())
        .bind(invoice.currency())
        .bind(invoice.hosted_url())
        .bind(invoice.pdf_url())
        .bind(period_start)
        .bind(period_end)
        .bind(invoice.created_at())
        .bind(invoice.paid_at())
        .bind(event_created)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<InvoiceRef>, BillingError> {
        let rows: Vec<InvoiceRow> = sqlx::query_as(
            "SELECT id, tenant_id, stripe_invoice_id, number, status, total_minor, currency,
                    hosted_url, pdf_url, period_start, period_end, created_at, paid_at
             FROM billing.invoices
             WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
             ORDER BY id DESC LIMIT $3",
        )
        .bind(tenant_id.as_i64())
        .bind(after_id)
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_invoice).collect()
    }

    async fn list_for_tenant_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<InvoiceRef>, BillingError> {
        let rows: Vec<InvoiceRow> = sqlx::query_as(
            "SELECT id, tenant_id, stripe_invoice_id, number, status, total_minor, currency,
                    hosted_url, pdf_url, period_start, period_end, created_at, paid_at
             FROM billing.invoices
             WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
             ORDER BY id DESC LIMIT $3",
        )
        .bind(tenant_id.as_i64())
        .bind(after_id)
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        rows.into_iter().map(to_invoice).collect()
    }

    async fn find_by_stripe_invoice_id(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_invoice_id: &str,
    ) -> Result<Option<InvoiceRef>, BillingError> {
        let row: Option<InvoiceRow> = sqlx::query_as(
            "SELECT id, tenant_id, stripe_invoice_id, number, status, total_minor, currency,
                    hosted_url, pdf_url, period_start, period_end, created_at, paid_at
             FROM billing.invoices WHERE stripe_invoice_id = $1",
        )
        .bind(stripe_invoice_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        row.map(to_invoice).transpose()
    }
}

// ---------------------------------------------------------------------------
// Webhook events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct WebhookRow {
    stripe_event_id: String,
    #[sqlx(rename = "type")]
    event_type: String,
    api_version: Option<String>,
    payload: serde_json::Value,
    created: DateTime<Utc>,
    received_at: DateTime<Utc>,
    attempts: i32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxWebhookEventRepository;

impl SqlxWebhookEventRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl WebhookEventRepository for SqlxWebhookEventRepository {
    async fn scrub_payloads_for_customer(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_customer_id: &str,
    ) -> Result<usize, BillingError> {
        // Raw event payloads mention the customer in several shapes; the
        // payload itself is Stripe's, not ours — replace any payload that
        // references the customer id with a redaction marker.
        let result = sqlx::query(
            "UPDATE billing.webhook_events
             SET payload = '{\"redacted\": true}'::jsonb, error = 'redacted by privacy erasure (phase 05)'
             WHERE payload::text LIKE '%' || $1 || '%'",
        )
        .bind(stripe_customer_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(result.rows_affected() as usize)
    }

    async fn insert_ignore(
        &self,
        scope: &mut PlatformScope<'_>,
        record: &WebhookEventRecord,
    ) -> Result<bool, BillingError> {
        // TASK-028: the history table is monthly-partitioned, so its primary
        // key carries the range key and can no longer arbitrate global
        // event-id uniqueness. The compact `webhook_event_dedup` sidecar
        // owns the claim (phase-10 conversion_event_dedup pattern): the CTE
        // claims first and inserts history only for the claimer, in one
        // statement inside the caller's transaction.
        let result = sqlx::query(
            "WITH claim AS (
               INSERT INTO billing.webhook_event_dedup (stripe_event_id)
               VALUES ($1)
               ON CONFLICT (stripe_event_id) DO NOTHING
               RETURNING stripe_event_id
             )
             INSERT INTO billing.webhook_events
               (stripe_event_id, type, api_version, payload, created, received_at)
             SELECT stripe_event_id, $2, $3, $4, $5, now() FROM claim",
        )
        .bind(&record.stripe_event_id)
        .bind(&record.event_type)
        .bind(&record.api_version)
        .bind(&record.payload)
        .bind(record.created)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(result.rows_affected() > 0)
    }

    async fn fetch_unprocessed(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<WebhookEventRecord>, BillingError> {
        let rows: Vec<WebhookRow> = sqlx::query_as(
            "SELECT stripe_event_id, type, api_version, payload, created, received_at, attempts
             FROM billing.webhook_events
             WHERE processed_at IS NULL AND attempts < 5
             ORDER BY received_at LIMIT $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(rows
            .into_iter()
            .map(|row| WebhookEventRecord {
                stripe_event_id: row.stripe_event_id,
                event_type: row.event_type,
                api_version: row.api_version,
                payload: row.payload,
                created: row.created,
                received_at: row.received_at,
                attempts: row.attempts,
            })
            .collect())
    }

    async fn mark_processed(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
    ) -> Result<(), BillingError> {
        sqlx::query(
            "UPDATE billing.webhook_events SET processed_at = now(), error = NULL
             WHERE stripe_event_id = $1",
        )
        .bind(stripe_event_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn mark_attempt_failed(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
        error: &str,
    ) -> Result<(), BillingError> {
        sqlx::query(
            "UPDATE billing.webhook_events SET attempts = attempts + 1, error = $2
             WHERE stripe_event_id = $1",
        )
        .bind(stripe_event_id)
        .bind(error)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn unprocessed_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, BillingError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing.webhook_events
             WHERE processed_at IS NULL AND attempts < 5",
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(count)
    }

    async fn dead_letter_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, BillingError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing.webhook_events
             WHERE processed_at IS NULL AND attempts >= 5",
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_db)?;
        Ok(count)
    }
}
