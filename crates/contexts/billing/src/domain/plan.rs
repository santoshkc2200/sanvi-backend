//! Plans, prices and the plan → entitlement map. The map is data, not code:
//! pricing changes never require a deploy; the recompute service derives
//! tenant access from these rows.

use chrono::{DateTime, Utc};
use sanvi_kernel::Id;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::BillingError;

pub enum PlanTag {}
pub type PlanId = Id<PlanTag>;

pub enum PriceTag {}
pub type PriceId = Id<PriceTag>;

/// One sellable tier. Each tier maps to exactly one Stripe Product so
/// Checkout line items show a distinguishable name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanTier {
    Starter,
    Professional,
    Enterprise,
}

impl PlanTier {
    pub fn parse(raw: &str) -> Result<Self, BillingError> {
        match raw.to_ascii_lowercase().as_str() {
            "starter" => Ok(Self::Starter),
            "professional" => Ok(Self::Professional),
            "enterprise" => Ok(Self::Enterprise),
            other => Err(BillingError::InvalidPlanTier(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starter => "starter",
            Self::Professional => "professional",
            Self::Enterprise => "enterprise",
        }
    }
}

/// Whether a plan appears on the public pricing page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanVisibility {
    #[default]
    Public,
    Hidden,
}

impl PlanVisibility {
    pub fn parse(raw: &str) -> Result<Self, BillingError> {
        match raw.to_ascii_lowercase().as_str() {
            "public" => Ok(Self::Public),
            "hidden" => Ok(Self::Hidden),
            other => Err(BillingError::InvalidPlanVisibility(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Hidden => "hidden",
        }
    }
}

/// The plan aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    id: PlanId,
    key: String,
    name: String,
    tier: PlanTier,
    stripe_product_id: String,
    visibility: PlanVisibility,
    sort_order: i32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl Plan {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PlanId,
        key: String,
        name: String,
        tier: PlanTier,
        stripe_product_id: String,
        visibility: PlanVisibility,
        sort_order: i32,
        now: DateTime<Utc>,
    ) -> Result<Self, BillingError> {
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty()
            || key.len() > 63
            || !key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(BillingError::InvalidPlanKey(key));
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(BillingError::InvalidPlanName(name.to_string()));
        }
        if stripe_product_id.trim().is_empty() {
            return Err(BillingError::InvalidPrice(
                "stripe_product_id must not be empty".to_string(),
            ));
        }
        Ok(Self {
            id,
            key,
            name: name.to_string(),
            tier,
            stripe_product_id,
            visibility,
            sort_order,
            created_at: now,
            updated_at: now,
        })
    }

    /// Re-hydration constructor (no validation; rows came from us).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: PlanId,
        key: String,
        name: String,
        tier: PlanTier,
        stripe_product_id: String,
        visibility: PlanVisibility,
        sort_order: i32,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            key,
            name,
            tier,
            stripe_product_id,
            visibility,
            sort_order,
            created_at,
            updated_at,
        }
    }

    pub fn id(&self) -> PlanId {
        self.id
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn tier(&self) -> PlanTier {
        self.tier
    }
    pub fn stripe_product_id(&self) -> &str {
        &self.stripe_product_id
    }
    pub fn visibility(&self) -> PlanVisibility {
        self.visibility
    }
    pub fn sort_order(&self) -> i32 {
        self.sort_order
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    /// Update the mutable fields (key/tier/product are stable once created;
    /// renaming those is a product decision that creates a new plan).
    pub fn update(
        &mut self,
        name: String,
        visibility: PlanVisibility,
        sort_order: i32,
        now: DateTime<Utc>,
    ) -> Result<(), BillingError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(BillingError::InvalidPlanName(name.to_string()));
        }
        self.name = name.to_string();
        self.visibility = visibility;
        self.sort_order = sort_order;
        self.updated_at = now;
        Ok(())
    }
}

/// Billing variant of a plan: monthly vs annual, currency, amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PriceInterval {
    Month,
    Year,
}

impl PriceInterval {
    pub fn parse(raw: &str) -> Result<Self, BillingError> {
        match raw.to_ascii_lowercase().as_str() {
            "month" => Ok(Self::Month),
            "year" => Ok(Self::Year),
            other => Err(BillingError::InvalidInterval(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// A Stripe Price object attached to one plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPrice {
    id: PriceId,
    plan_id: PlanId,
    stripe_price_id: String,
    currency: String,
    interval: PriceInterval,
    unit_amount_minor: i64,
    trial_days: Option<u16>,
    active: bool,
    created_at: DateTime<Utc>,
}

impl PlanPrice {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PriceId,
        plan_id: PlanId,
        stripe_price_id: String,
        currency: String,
        interval: PriceInterval,
        unit_amount_minor: i64,
        trial_days: Option<u16>,
        now: DateTime<Utc>,
    ) -> Result<Self, BillingError> {
        let currency = currency.trim().to_ascii_lowercase();
        if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_lowercase()) {
            return Err(BillingError::InvalidCurrency(currency));
        }
        if unit_amount_minor < 0 {
            return Err(BillingError::InvalidPrice(
                "unit_amount_minor must be >= 0".to_string(),
            ));
        }
        if stripe_price_id.trim().is_empty() {
            return Err(BillingError::InvalidPrice(
                "stripe_price_id must not be empty".to_string(),
            ));
        }
        Ok(Self {
            id,
            plan_id,
            stripe_price_id,
            currency,
            interval,
            unit_amount_minor,
            trial_days,
            active: true,
            created_at: now,
        })
    }

    /// Re-hydration constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: PriceId,
        plan_id: PlanId,
        stripe_price_id: String,
        currency: String,
        interval: PriceInterval,
        unit_amount_minor: i64,
        trial_days: Option<u16>,
        active: bool,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            plan_id,
            stripe_price_id,
            currency,
            interval,
            unit_amount_minor,
            trial_days,
            active,
            created_at,
        }
    }

    pub fn id(&self) -> PriceId {
        self.id
    }
    pub fn plan_id(&self) -> PlanId {
        self.plan_id
    }
    pub fn stripe_price_id(&self) -> &str {
        &self.stripe_price_id
    }
    pub fn currency(&self) -> &str {
        &self.currency
    }
    pub fn interval(&self) -> PriceInterval {
        self.interval
    }
    pub fn unit_amount_minor(&self) -> i64 {
        self.unit_amount_minor
    }
    pub fn trial_days(&self) -> Option<u16> {
        self.trial_days
    }
    pub fn active(&self) -> bool {
        self.active
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

/// One plan → feature grant. Feature keys reference the access context's
/// catalog (validated at write time; no FK because the catalog is runtime
/// data).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EntitlementGrant {
    pub feature_key: String,
    pub enabled: bool,
    pub limit: Option<i64>,
}

impl EntitlementGrant {
    pub fn new(
        feature_key: String,
        enabled: bool,
        limit: Option<i64>,
    ) -> Result<Self, BillingError> {
        let feature_key = feature_key.trim().to_ascii_lowercase();
        if feature_key.is_empty() || !feature_key.contains('.') {
            return Err(BillingError::InvalidEntitlement(feature_key));
        }
        if let Some(limit) = limit
            && limit < 0
        {
            return Err(BillingError::InvalidEntitlement(format!(
                "negative limit for {feature_key}"
            )));
        }
        Ok(Self {
            feature_key,
            enabled,
            limit,
        })
    }
}

/// The webhook parser's reference to a tenant (subscription metadata set at
/// Checkout creation).
pub(crate) const TENANT_METADATA_KEY: &str = "sanvi_tenant_id";

/// Resolve a tenant id from Stripe metadata (subscription or customer).
pub(crate) fn tenant_id_from_metadata(metadata: &serde_json::Value) -> Option<Uuid> {
    metadata
        .get(TENANT_METADATA_KEY)
        .and_then(|v| v.as_str())
        .and_then(|raw| Uuid::parse_str(raw).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn plan_keys_are_normalized_and_validated() {
        let plan = Plan::new(
            PlanId::new(),
            " Professional ".to_string(),
            "Professional".to_string(),
            PlanTier::Professional,
            "prod_x".to_string(),
            PlanVisibility::Public,
            2,
            now(),
        )
        .unwrap();
        assert_eq!(plan.key(), "professional");
        assert!(
            Plan::new(
                PlanId::new(),
                "bad key!".to_string(),
                "x".to_string(),
                PlanTier::Starter,
                "prod_x".to_string(),
                PlanVisibility::Public,
                1,
                now(),
            )
            .is_err()
        );
    }

    #[test]
    fn prices_enforce_iso_currency_and_nonnegative_amounts() {
        assert!(
            PlanPrice::new(
                PriceId::new(),
                PlanId::new(),
                "price_x".to_string(),
                "USD".to_string(),
                PriceInterval::Month,
                4900,
                None,
                now(),
            )
            .is_ok()
        );
        assert!(
            PlanPrice::new(
                PriceId::new(),
                PlanId::new(),
                "price_x".to_string(),
                "us".to_string(),
                PriceInterval::Month,
                4900,
                None,
                now(),
            )
            .is_err()
        );
        assert!(
            PlanPrice::new(
                PriceId::new(),
                PlanId::new(),
                "price_x".to_string(),
                "usd".to_string(),
                PriceInterval::Month,
                -1,
                None,
                now(),
            )
            .is_err()
        );
    }

    #[test]
    fn entitlement_grants_reject_garbage() {
        assert!(EntitlementGrant::new("tenancy.member_limit".to_string(), true, Some(5)).is_ok());
        assert!(EntitlementGrant::new("nokey".to_string(), true, None).is_err());
        assert!(EntitlementGrant::new("tenancy.member_limit".to_string(), true, Some(-1)).is_err());
    }

    #[test]
    fn tier_and_interval_parse_normalize_case() {
        assert_eq!(
            PlanTier::parse("PROFESSIONAL").unwrap(),
            PlanTier::Professional
        );
        assert_eq!(PriceInterval::parse("Month").unwrap(), PriceInterval::Month);
        assert!(PlanTier::parse("free").is_err());
    }

    #[test]
    fn tenant_id_from_metadata_accepts_uuids_only() {
        assert_eq!(
            tenant_id_from_metadata(&serde_json::json!({ TENANT_METADATA_KEY: "not-a-uuid" })),
            None
        );
        let id = Uuid::now_v7();
        assert_eq!(
            tenant_id_from_metadata(&serde_json::json!({
                TENANT_METADATA_KEY: id.to_string()
            })),
            Some(id)
        );
    }
}
