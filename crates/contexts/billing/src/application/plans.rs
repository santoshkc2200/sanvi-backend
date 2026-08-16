//! Plan catalog use cases: the public pricing page, the operator CRUD for
//! plans/prices/entitlement maps, and the validation that keeps feature
//! grants pointing at real catalog keys.

use std::sync::Arc;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_persistence::TxManager;
use serde::Serialize;
use tracing::info;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::storage;
use crate::domain::{
    BillingError, Clock, EntitlementGrant, IdGenerator, Plan, PlanEntitlementRepository, PlanId,
    PlanPrice, PlanPriceRepository, PlanRepository, PlanTier, PlanVisibility, PriceInterval,
};

const ACTOR: &str = "platform-admin";

async fn audit_plan(
    audit: &dyn AuditWriter,
    scope: &mut sanvi_persistence::PlatformScope<'_>,
    actor: Uuid,
    action: &str,
    plan: &Plan,
    request_id: Option<String>,
) -> Result<(), BillingError> {
    audit
        .append(
            scope,
            &AuditEntryData {
                occurred_at: chrono::Utc::now(),
                actor_type: ActorType::PlatformAdmin,
                actor_id: Some(actor),
                on_behalf_of: None,
                tenant_id: None,
                action: action.to_string(),
                resource_type: Some("plan".to_string()),
                resource_id: Some(plan.key().to_string()),
                before: None,
                after: Some(serde_json::json!({ "plan_id": plan.id().as_uuid() })),
                request_id,
                ip: None,
                user_agent: None,
            },
        )
        .await
        .map(|_| ())
        .map_err(|err| BillingError::Storage(err.to_string()))
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// One price on the public pricing page.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlanPriceView {
    pub price_id: Uuid,
    pub currency: String,
    pub interval: PriceInterval,
    pub unit_amount_minor: i64,
    pub trial_days: Option<u16>,
}

impl From<&PlanPrice> for PlanPriceView {
    fn from(price: &PlanPrice) -> Self {
        Self {
            price_id: price.id().as_uuid(),
            currency: price.currency().to_string(),
            interval: price.interval(),
            unit_amount_minor: price.unit_amount_minor(),
            trial_days: price.trial_days(),
        }
    }
}

/// A plan with its prices and entitlement grants (public pricing page; the
/// Stripe price ids are omitted — they are integration details).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicPlanView {
    pub plan_id: Uuid,
    pub key: String,
    pub name: String,
    pub tier: PlanTier,
    pub sort_order: i32,
    pub prices: Vec<PlanPriceView>,
    pub entitlements: Vec<EntitlementGrant>,
}

/// The operator view adds the Stripe ids and visibility.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlanAdminView {
    pub plan_id: Uuid,
    pub key: String,
    pub name: String,
    pub tier: PlanTier,
    pub stripe_product_id: String,
    pub visibility: PlanVisibility,
    pub sort_order: i32,
    pub prices: Vec<PlanAdminPriceView>,
    pub entitlements: Vec<EntitlementGrant>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlanAdminPriceView {
    pub price_id: Uuid,
    pub stripe_price_id: String,
    pub currency: String,
    pub interval: PriceInterval,
    pub unit_amount_minor: i64,
    pub trial_days: Option<u16>,
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// The public pricing page (localized by phase 06; plans hidden from the
/// public catalog are excluded).
pub struct ListPublicPlans {
    tx_manager: TxManager,
    plans: Arc<dyn PlanRepository>,
    prices: Arc<dyn PlanPriceRepository>,
    grants: Arc<dyn PlanEntitlementRepository>,
}

impl ListPublicPlans {
    pub fn new(
        tx_manager: TxManager,
        plans: Arc<dyn PlanRepository>,
        prices: Arc<dyn PlanPriceRepository>,
        grants: Arc<dyn PlanEntitlementRepository>,
    ) -> Self {
        Self {
            tx_manager,
            plans,
            prices,
            grants,
        }
    }

    pub async fn execute(&self) -> Result<Vec<PublicPlanView>, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_public_plans", "public")
            .await
            .map_err(storage)?;
        let plans = self.plans.list(&mut scope, true).await?;
        let mut views = Vec::with_capacity(plans.len());
        for plan in plans {
            let prices = self
                .prices
                .list_active_for_plan(&mut scope, plan.id())
                .await?
                .iter()
                .map(PlanPriceView::from)
                .collect();
            let entitlements = self.grants.list_for_plan(&mut scope, plan.id()).await?;
            views.push(PublicPlanView {
                plan_id: plan.id().as_uuid(),
                key: plan.key().to_string(),
                name: plan.name().to_string(),
                tier: plan.tier(),
                sort_order: plan.sort_order(),
                prices,
                entitlements,
            });
        }
        scope.commit().await.map_err(storage)?;
        Ok(views)
    }
}

/// The operator catalog: every plan, including hidden ones.
pub struct ListPlans {
    tx_manager: TxManager,
    plans: Arc<dyn PlanRepository>,
    prices: Arc<dyn PlanPriceRepository>,
    grants: Arc<dyn PlanEntitlementRepository>,
}

impl ListPlans {
    pub fn new(
        tx_manager: TxManager,
        plans: Arc<dyn PlanRepository>,
        prices: Arc<dyn PlanPriceRepository>,
        grants: Arc<dyn PlanEntitlementRepository>,
    ) -> Self {
        Self {
            tx_manager,
            plans,
            prices,
            grants,
        }
    }

    pub async fn execute(&self) -> Result<Vec<PlanAdminView>, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_plans", ACTOR)
            .await
            .map_err(storage)?;
        let plans = self.plans.list(&mut scope, false).await?;
        let mut views = Vec::with_capacity(plans.len());
        for plan in plans {
            let prices = self
                .prices
                .list_active_for_plan(&mut scope, plan.id())
                .await?;
            let entitlements = self.grants.list_for_plan(&mut scope, plan.id()).await?;
            views.push(PlanAdminView {
                plan_id: plan.id().as_uuid(),
                key: plan.key().to_string(),
                name: plan.name().to_string(),
                tier: plan.tier(),
                stripe_product_id: plan.stripe_product_id().to_string(),
                visibility: plan.visibility(),
                sort_order: plan.sort_order(),
                prices: prices
                    .iter()
                    .map(|price| PlanAdminPriceView {
                        price_id: price.id().as_uuid(),
                        stripe_price_id: price.stripe_price_id().to_string(),
                        currency: price.currency().to_string(),
                        interval: price.interval(),
                        unit_amount_minor: price.unit_amount_minor(),
                        trial_days: price.trial_days(),
                        active: price.active(),
                    })
                    .collect(),
                entitlements,
            });
        }
        scope.commit().await.map_err(storage)?;
        Ok(views)
    }
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

pub struct CreatePlanCommand {
    pub key: String,
    pub name: String,
    pub tier: PlanTier,
    pub stripe_product_id: String,
    pub visibility: PlanVisibility,
    pub sort_order: i32,
}

/// Create a plan (one Stripe Product per tier, created in the Dashboard
/// first; the operator pastes its id here).
pub struct CreatePlan {
    tx_manager: TxManager,
    plans: Arc<dyn PlanRepository>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl CreatePlan {
    pub fn new(
        tx_manager: TxManager,
        plans: Arc<dyn PlanRepository>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            plans,
            ids,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        cmd: CreatePlanCommand,
    ) -> Result<PlanAdminView, BillingError> {
        let now = self.clock.now();
        let mut scope = self
            .tx_manager
            .begin_unscoped("create_plan", ACTOR)
            .await
            .map_err(storage)?;
        let plan = Plan::new(
            self.ids.next_plan_id(),
            cmd.key.clone(),
            cmd.name,
            cmd.tier,
            cmd.stripe_product_id,
            cmd.visibility,
            cmd.sort_order,
            now,
        )?;
        if self
            .plans
            .find_by_key(&mut scope, plan.key())
            .await?
            .is_some()
        {
            return Err(BillingError::PlanKeyTaken(plan.key().to_string()));
        }
        self.plans.save(&mut scope, &plan).await?;
        audit_plan(
            self.audit.as_ref(),
            &mut scope,
            actor,
            "billing.plan_created",
            &plan,
            None,
        )
        .await?;
        scope.commit().await.map_err(storage)?;
        info!(plan = plan.key(), "plan created");
        Ok(PlanAdminView {
            plan_id: plan.id().as_uuid(),
            key: plan.key().to_string(),
            name: plan.name().to_string(),
            tier: plan.tier(),
            stripe_product_id: plan.stripe_product_id().to_string(),
            visibility: plan.visibility(),
            sort_order: plan.sort_order(),
            prices: vec![],
            entitlements: vec![],
        })
    }
}

pub struct UpdatePlanCommand {
    pub plan_id: PlanId,
    pub name: String,
    pub visibility: PlanVisibility,
    pub sort_order: i32,
}

/// Update a plan's mutable fields (name, visibility, sort order).
pub struct UpdatePlan {
    tx_manager: TxManager,
    plans: Arc<dyn PlanRepository>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl UpdatePlan {
    pub fn new(
        tx_manager: TxManager,
        plans: Arc<dyn PlanRepository>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            plans,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        cmd: UpdatePlanCommand,
    ) -> Result<PlanAdminView, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("update_plan", ACTOR)
            .await
            .map_err(storage)?;
        let mut plan = self
            .plans
            .find_by_id(&mut scope, cmd.plan_id)
            .await?
            .ok_or_else(|| BillingError::PlanNotFound(cmd.plan_id.to_string()))?;
        plan.update(cmd.name, cmd.visibility, cmd.sort_order, self.clock.now())?;
        self.plans.save(&mut scope, &plan).await?;
        audit_plan(
            self.audit.as_ref(),
            &mut scope,
            actor,
            "billing.plan_updated",
            &plan,
            None,
        )
        .await?;
        scope.commit().await.map_err(storage)?;
        Ok(PlanAdminView {
            plan_id: plan.id().as_uuid(),
            key: plan.key().to_string(),
            name: plan.name().to_string(),
            tier: plan.tier(),
            stripe_product_id: plan.stripe_product_id().to_string(),
            visibility: plan.visibility(),
            sort_order: plan.sort_order(),
            prices: vec![],
            entitlements: vec![],
        })
    }
}

pub struct AddPlanPriceCommand {
    pub plan_id: PlanId,
    pub stripe_price_id: String,
    pub currency: String,
    pub interval: PriceInterval,
    pub unit_amount_minor: i64,
    pub trial_days: Option<u16>,
}

/// Attach a Stripe Price to a plan (one product per plan; prices are the
/// billing variants).
pub struct AddPlanPrice {
    tx_manager: TxManager,
    plans: Arc<dyn PlanRepository>,
    prices: Arc<dyn PlanPriceRepository>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl AddPlanPrice {
    pub fn new(
        tx_manager: TxManager,
        plans: Arc<dyn PlanRepository>,
        prices: Arc<dyn PlanPriceRepository>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            plans,
            prices,
            ids,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        cmd: AddPlanPriceCommand,
    ) -> Result<PlanAdminPriceView, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("add_plan_price", ACTOR)
            .await
            .map_err(storage)?;
        let plan = self
            .plans
            .find_by_id(&mut scope, cmd.plan_id)
            .await?
            .ok_or_else(|| BillingError::PlanNotFound(cmd.plan_id.to_string()))?;
        let price = PlanPrice::new(
            self.ids.next_price_id(),
            plan.id(),
            cmd.stripe_price_id,
            cmd.currency,
            cmd.interval,
            cmd.unit_amount_minor,
            cmd.trial_days,
            self.clock.now(),
        )?;
        self.prices.save(&mut scope, &price).await?;
        audit_plan(
            self.audit.as_ref(),
            &mut scope,
            actor,
            "billing.plan_price_added",
            &plan,
            None,
        )
        .await?;
        scope.commit().await.map_err(storage)?;
        Ok(PlanAdminPriceView {
            price_id: price.id().as_uuid(),
            stripe_price_id: price.stripe_price_id().to_string(),
            currency: price.currency().to_string(),
            interval: price.interval(),
            unit_amount_minor: price.unit_amount_minor(),
            trial_days: price.trial_days(),
            active: price.active(),
        })
    }
}

pub struct SetPlanEntitlementsCommand {
    pub plan_id: PlanId,
    pub grants: Vec<EntitlementGrant>,
}

/// Replace a plan's entitlement map. Feature keys are validated against the
/// access context's live catalog (`features_repo`) so grants can never point
/// at unknown capabilities.
pub struct SetPlanEntitlements {
    tx_manager: TxManager,
    plans: Arc<dyn PlanRepository>,
    grants_repo: Arc<dyn PlanEntitlementRepository>,
    features: Arc<dyn sanvi_access::domain::FeatureRepository>,
    audit: Arc<dyn AuditWriter>,
}

impl SetPlanEntitlements {
    pub fn new(
        tx_manager: TxManager,
        plans: Arc<dyn PlanRepository>,
        grants_repo: Arc<dyn PlanEntitlementRepository>,
        features: Arc<dyn sanvi_access::domain::FeatureRepository>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            plans,
            grants_repo,
            features,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        cmd: SetPlanEntitlementsCommand,
    ) -> Result<Vec<EntitlementGrant>, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("set_plan_entitlements", ACTOR)
            .await
            .map_err(storage)?;
        let plan = self
            .plans
            .find_by_id(&mut scope, cmd.plan_id)
            .await?
            .ok_or_else(|| BillingError::PlanNotFound(cmd.plan_id.to_string()))?;
        for grant in &cmd.grants {
            let key =
                sanvi_access::domain::FeatureKey::parse(&grant.feature_key).map_err(|err| {
                    BillingError::InvalidEntitlement(format!("{}: {err}", grant.feature_key))
                })?;
            if self
                .features
                .find(&mut scope, &key)
                .await
                .map_err(|err| BillingError::Storage(err.to_string()))?
                .is_none()
            {
                return Err(BillingError::InvalidEntitlement(format!(
                    "feature not in catalog: {}",
                    grant.feature_key
                )));
            }
        }
        self.grants_repo
            .replace_for_plan(&mut scope, plan.id(), &cmd.grants)
            .await?;
        audit_plan(
            self.audit.as_ref(),
            &mut scope,
            actor,
            "billing.plan_entitlements_set",
            &plan,
            None,
        )
        .await?;
        scope.commit().await.map_err(storage)?;
        info!(
            plan = plan.key(),
            grants = cmd.grants.len(),
            "plan entitlement map replaced"
        );
        Ok(cmd.grants)
    }
}
