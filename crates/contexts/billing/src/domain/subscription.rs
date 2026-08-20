//! Subscriptions: the local projection of the tenant's Stripe subscription,
//! plus the dunning collection state machine. `SubscriptionStatus` mirrors
//! Stripe exactly — deliberately not re-invented, so webhook mapping is
//! total.

use chrono::{DateTime, Utc};
use sanvi_kernel::Id;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::BillingError;
use crate::domain::plan::{PlanId, PriceId};

pub enum SubscriptionTag {}
pub type SubscriptionId = Id<SubscriptionTag>;

/// Mirrors Stripe's subscription status one-to-one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Trialing,
    Active,
    PastDue,
    Unpaid,
    Canceled,
    Incomplete,
    IncompleteExpired,
    Paused,
}

impl SubscriptionStatus {
    pub fn parse(raw: &str) -> Result<Self, BillingError> {
        match raw {
            "trialing" => Ok(Self::Trialing),
            "active" => Ok(Self::Active),
            "past_due" => Ok(Self::PastDue),
            "unpaid" => Ok(Self::Unpaid),
            "canceled" => Ok(Self::Canceled),
            "incomplete" => Ok(Self::Incomplete),
            "incomplete_expired" => Ok(Self::IncompleteExpired),
            "paused" => Ok(Self::Paused),
            other => Err(BillingError::InvalidWebhookPayload(format!(
                "unknown subscription status: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trialing => "trialing",
            Self::Active => "active",
            Self::PastDue => "past_due",
            Self::Unpaid => "unpaid",
            Self::Canceled => "canceled",
            Self::Incomplete => "incomplete",
            Self::IncompleteExpired => "incomplete_expired",
            Self::Paused => "paused",
        }
    }

    /// Whether this status grants the plan's entitlements. Past-due keeps
    /// access during the dunning grace window; everything else denies.
    pub fn grants_entitlements(self) -> bool {
        matches!(self, Self::Trialing | Self::Active | Self::PastDue)
    }
}

/// Our dunning machine, orthogonal to Stripe's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CollectionState {
    Ok,
    /// A payment failed; the grace window is running.
    Dunning,
    /// The grace window expired; entitlements were downgraded and the
    /// tenant was suspended.
    GraceExpired,
}

impl CollectionState {
    pub fn parse(raw: &str) -> Result<Self, BillingError> {
        match raw {
            "ok" => Ok(Self::Ok),
            "dunning" => Ok(Self::Dunning),
            "grace_expired" => Ok(Self::GraceExpired),
            other => Err(BillingError::InvalidWebhookPayload(format!(
                "unknown collection state: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Dunning => "dunning",
            Self::GraceExpired => "grace_expired",
        }
    }
}

/// Where the subscription came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionSource {
    /// Backed by a real Stripe subscription.
    Stripe,
    /// Operator-granted (no Stripe object; reconciliation skips it).
    Override,
}

impl SubscriptionSource {
    pub fn parse(raw: &str) -> Result<Self, BillingError> {
        match raw {
            "stripe" => Ok(Self::Stripe),
            "override" => Ok(Self::Override),
            other => Err(BillingError::InvalidWebhookPayload(format!(
                "unknown subscription source: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stripe => "stripe",
            Self::Override => "override",
        }
    }
}

/// A billing period (current or invoice period).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Period {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// The subscription aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    id: SubscriptionId,
    tenant_id: Uuid,
    plan_id: PlanId,
    price_id: Option<PriceId>,
    source: SubscriptionSource,
    stripe_customer_id: Option<String>,
    stripe_subscription_id: Option<String>,
    status: SubscriptionStatus,
    current_period: Option<Period>,
    cancel_at_period_end: bool,
    trial_end: Option<DateTime<Utc>>,
    trial_notified_at: Option<DateTime<Utc>>,
    collection_state: CollectionState,
    collection_started_at: Option<DateTime<Utc>>,
    stripe_subscription_event_at: Option<DateTime<Utc>>,
    stripe_invoice_event_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl Subscription {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: SubscriptionId,
        tenant_id: Uuid,
        plan_id: PlanId,
        price_id: Option<PriceId>,
        source: SubscriptionSource,
        stripe_customer_id: Option<String>,
        stripe_subscription_id: Option<String>,
        status: SubscriptionStatus,
        now: DateTime<Utc>,
    ) -> Result<Self, BillingError> {
        let stripe = source == SubscriptionSource::Stripe;
        if stripe
            && (stripe_customer_id.is_none()
                || stripe_subscription_id.is_none()
                || price_id.is_none())
        {
            return Err(BillingError::InvalidWebhookPayload(
                "stripe subscriptions carry customer id, subscription id and price id".to_string(),
            ));
        }
        Ok(Self {
            id,
            tenant_id,
            plan_id,
            price_id,
            source,
            stripe_customer_id,
            stripe_subscription_id,
            status,
            current_period: None,
            cancel_at_period_end: false,
            trial_end: None,
            trial_notified_at: None,
            collection_state: CollectionState::Ok,
            collection_started_at: None,
            stripe_subscription_event_at: None,
            stripe_invoice_event_at: None,
            created_at: now,
            updated_at: now,
        })
    }

    /// Re-hydration constructor (no validation; rows came from us).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: SubscriptionId,
        tenant_id: Uuid,
        plan_id: PlanId,
        price_id: Option<PriceId>,
        source: SubscriptionSource,
        stripe_customer_id: Option<String>,
        stripe_subscription_id: Option<String>,
        status: SubscriptionStatus,
        current_period: Option<Period>,
        cancel_at_period_end: bool,
        trial_end: Option<DateTime<Utc>>,
        trial_notified_at: Option<DateTime<Utc>>,
        collection_state: CollectionState,
        collection_started_at: Option<DateTime<Utc>>,
        stripe_subscription_event_at: Option<DateTime<Utc>>,
        stripe_invoice_event_at: Option<DateTime<Utc>>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            tenant_id,
            plan_id,
            price_id,
            source,
            stripe_customer_id,
            stripe_subscription_id,
            status,
            current_period,
            cancel_at_period_end,
            trial_end,
            trial_notified_at,
            collection_state,
            collection_started_at,
            stripe_subscription_event_at,
            stripe_invoice_event_at,
            created_at,
            updated_at,
        }
    }

    pub fn id(&self) -> SubscriptionId {
        self.id
    }
    pub fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }
    pub fn plan_id(&self) -> PlanId {
        self.plan_id
    }
    pub fn price_id(&self) -> Option<PriceId> {
        self.price_id
    }
    pub fn source(&self) -> SubscriptionSource {
        self.source
    }
    pub fn stripe_customer_id(&self) -> Option<&str> {
        self.stripe_customer_id.as_deref()
    }
    pub fn stripe_subscription_id(&self) -> Option<&str> {
        self.stripe_subscription_id.as_deref()
    }
    pub fn status(&self) -> SubscriptionStatus {
        self.status
    }
    pub fn current_period(&self) -> Option<Period> {
        self.current_period
    }
    pub fn cancel_at_period_end(&self) -> bool {
        self.cancel_at_period_end
    }
    pub fn trial_end(&self) -> Option<DateTime<Utc>> {
        self.trial_end
    }
    pub fn trial_notified_at(&self) -> Option<DateTime<Utc>> {
        self.trial_notified_at
    }
    pub fn collection_state(&self) -> CollectionState {
        self.collection_state
    }
    pub fn collection_started_at(&self) -> Option<DateTime<Utc>> {
        self.collection_started_at
    }
    pub fn stripe_subscription_event_at(&self) -> Option<DateTime<Utc>> {
        self.stripe_subscription_event_at
    }
    pub fn stripe_invoice_event_at(&self) -> Option<DateTime<Utc>> {
        self.stripe_invoice_event_at
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    /// True when this subscription currently grants its plan's
    /// entitlements (status grants access and dunning has not expired).
    pub fn grants_entitlements(&self) -> bool {
        self.status.grants_entitlements() && self.collection_state != CollectionState::GraceExpired
    }

    /// Apply a state update from Stripe; returns whether the subscription's
    /// entitlement-bearing facts changed (plan, status, collection state).
    #[allow(clippy::too_many_arguments)]
    pub fn apply_stripe_update(
        &mut self,
        status: SubscriptionStatus,
        plan_id: PlanId,
        price_id: Option<PriceId>,
        current_period: Option<Period>,
        cancel_at_period_end: bool,
        trial_end: Option<DateTime<Utc>>,
        event_created: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        let mut entitlements_changed = false;
        if self.plan_id != plan_id || self.status != status {
            entitlements_changed = true;
        }
        self.plan_id = plan_id;
        if price_id.is_some() {
            self.price_id = price_id;
        }
        self.status = status;
        self.current_period = current_period;
        self.cancel_at_period_end = cancel_at_period_end;
        self.trial_end = trial_end;
        // Payment recovered / renewed: clear the dunning machine.
        if status == SubscriptionStatus::Active || status == SubscriptionStatus::Trialing {
            if self.collection_state != CollectionState::Ok {
                entitlements_changed = true;
            }
            self.collection_state = CollectionState::Ok;
            self.collection_started_at = None;
        }
        // A failed payment starts (or continues) dunning.
        if status == SubscriptionStatus::PastDue && self.collection_state == CollectionState::Ok {
            entitlements_changed = true;
            self.collection_state = CollectionState::Dunning;
            self.collection_started_at = Some(now);
        }
        self.stripe_subscription_event_at = Some(event_created);
        self.updated_at = now;
        entitlements_changed
    }

    /// Replace a terminal Stripe subscription with its successor while
    /// retaining the tenant-scoped local projection id.
    pub fn replace_stripe_identity(
        &mut self,
        customer_id: String,
        subscription_id: String,
    ) -> Result<(), BillingError> {
        if customer_id.trim().is_empty() || subscription_id.trim().is_empty() {
            return Err(BillingError::InvalidWebhookPayload(
                "replacement Stripe identity must not be empty".to_string(),
            ));
        }
        self.source = SubscriptionSource::Stripe;
        self.stripe_customer_id = Some(customer_id);
        self.stripe_subscription_id = Some(subscription_id);
        self.trial_notified_at = None;
        self.collection_state = CollectionState::Ok;
        self.collection_started_at = None;
        self.stripe_subscription_event_at = None;
        self.stripe_invoice_event_at = None;
        Ok(())
    }

    pub fn is_subscription_event_stale(&self, event_created: DateTime<Utc>) -> bool {
        self.stripe_subscription_event_at
            .is_some_and(|last| event_created < last)
    }

    pub fn is_invoice_event_stale(&self, event_created: DateTime<Utc>) -> bool {
        self.stripe_invoice_event_at
            .is_some_and(|last| event_created < last)
    }

    pub fn record_invoice_event(&mut self, event_created: DateTime<Utc>, now: DateTime<Utc>) {
        self.stripe_invoice_event_at = Some(event_created);
        self.updated_at = now;
    }

    pub fn start_dunning_from_invoice(
        &mut self,
        event_created: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        let changed = self.status != SubscriptionStatus::PastDue
            || self.collection_state != CollectionState::Dunning;
        self.status = SubscriptionStatus::PastDue;
        if self.collection_state == CollectionState::Ok {
            self.collection_state = CollectionState::Dunning;
            self.collection_started_at = Some(now);
        }
        self.record_invoice_event(event_created, now);
        changed
    }

    /// The dunning grace window expired: entitlements drop to the limited
    /// set and the tenant is suspended. Returns false when the subscription
    /// was not actually dunning (out-of-order sweep).
    pub fn expire_grace(&mut self, now: DateTime<Utc>) -> bool {
        if self.collection_state != CollectionState::Dunning {
            return false;
        }
        self.collection_state = CollectionState::GraceExpired;
        self.updated_at = now;
        true
    }

    /// A payment came in after grace expiry: full restoration.
    pub fn restore(&mut self, now: DateTime<Utc>) {
        self.collection_state = CollectionState::Ok;
        self.collection_started_at = None;
        self.updated_at = now;
    }

    /// Mark the trial-ending notice as sent.
    pub fn mark_trial_notified(&mut self, now: DateTime<Utc>) {
        self.trial_notified_at = Some(now);
        self.updated_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn sub(status: SubscriptionStatus, collection: CollectionState) -> Subscription {
        let mut s = Subscription::new(
            SubscriptionId::new(),
            Uuid::now_v7(),
            PlanId::from_uuid(Uuid::now_v7()),
            Some(PriceId::from_uuid(Uuid::now_v7())),
            SubscriptionSource::Stripe,
            Some("cus_x".to_string()),
            Some("sub_x".to_string()),
            status,
            now(),
        )
        .unwrap();
        s.collection_state = collection;
        if collection != CollectionState::Ok {
            s.collection_started_at = Some(now());
        }
        s
    }

    #[test]
    fn stripe_subscriptions_require_full_stripe_identity() {
        assert!(
            Subscription::new(
                SubscriptionId::new(),
                Uuid::now_v7(),
                PlanId::from_uuid(Uuid::now_v7()),
                None,
                SubscriptionSource::Stripe,
                Some("cus_x".to_string()),
                None,
                SubscriptionStatus::Active,
                now(),
            )
            .is_err()
        );
        assert!(
            Subscription::new(
                SubscriptionId::new(),
                Uuid::now_v7(),
                PlanId::from_uuid(Uuid::now_v7()),
                None,
                SubscriptionSource::Override,
                None,
                None,
                SubscriptionStatus::Active,
                now(),
            )
            .is_ok()
        );
    }

    #[test]
    fn status_mirrors_stripe_exactly() {
        for (raw, expected) in [
            ("trialing", SubscriptionStatus::Trialing),
            ("active", SubscriptionStatus::Active),
            ("past_due", SubscriptionStatus::PastDue),
            ("unpaid", SubscriptionStatus::Unpaid),
            ("canceled", SubscriptionStatus::Canceled),
            ("incomplete", SubscriptionStatus::Incomplete),
            ("incomplete_expired", SubscriptionStatus::IncompleteExpired),
            ("paused", SubscriptionStatus::Paused),
        ] {
            assert_eq!(SubscriptionStatus::parse(raw).unwrap(), expected);
            assert_eq!(expected.as_str(), raw);
        }
        assert!(SubscriptionStatus::parse("unknown").is_err());
    }

    #[test]
    fn only_access_granting_statuses_entitle() {
        assert!(SubscriptionStatus::Trialing.grants_entitlements());
        assert!(SubscriptionStatus::Active.grants_entitlements());
        assert!(SubscriptionStatus::PastDue.grants_entitlements());
        for status in [
            SubscriptionStatus::Unpaid,
            SubscriptionStatus::Canceled,
            SubscriptionStatus::Incomplete,
            SubscriptionStatus::IncompleteExpired,
            SubscriptionStatus::Paused,
        ] {
            assert!(!status.grants_entitlements(), "{status:?} must not entitle");
        }
    }

    #[test]
    fn dunning_flow_ends_in_grace_expiry_and_restores_on_payment() {
        let mut s = sub(SubscriptionStatus::Active, CollectionState::Ok);
        assert!(s.grants_entitlements());

        // Failed payment starts dunning (via apply_stripe_update).
        let changed = s.apply_stripe_update(
            SubscriptionStatus::PastDue,
            s.plan_id(),
            s.price_id(),
            s.current_period(),
            false,
            None,
            now(),
            now(),
        );
        assert!(changed);
        assert_eq!(s.collection_state(), CollectionState::Dunning);
        assert!(s.grants_entitlements(), "grace window still grants access");

        // Grace expiry downgrades.
        assert!(s.expire_grace(now()));
        assert_eq!(s.collection_state(), CollectionState::GraceExpired);
        assert!(!s.grants_entitlements());
        assert!(!s.expire_grace(now()), "expiry is idempotent");

        // Payment restores.
        s.restore(now());
        assert_eq!(s.collection_state(), CollectionState::Ok);
        assert!(s.grants_entitlements());
    }

    #[test]
    fn active_update_clears_dunning() {
        let mut s = sub(SubscriptionStatus::PastDue, CollectionState::Dunning);
        let changed = s.apply_stripe_update(
            SubscriptionStatus::Active,
            s.plan_id(),
            s.price_id(),
            s.current_period(),
            false,
            None,
            now(),
            now(),
        );
        assert!(changed);
        assert_eq!(s.collection_state(), CollectionState::Ok);
        assert_eq!(s.collection_started_at(), None);
    }

    #[test]
    fn plan_change_is_detected() {
        let mut s = sub(SubscriptionStatus::Active, CollectionState::Ok);
        let new_plan = PlanId::new();
        let changed = s.apply_stripe_update(
            SubscriptionStatus::Active,
            new_plan,
            s.price_id(),
            s.current_period(),
            false,
            None,
            now(),
            now(),
        );
        assert!(changed);
        assert_eq!(s.plan_id(), new_plan);
    }

    #[test]
    fn stale_stripe_updates_are_rejected() {
        let mut s = sub(SubscriptionStatus::Active, CollectionState::Ok);
        let provider_time = now() - chrono::Duration::minutes(10);
        s.apply_stripe_update(
            s.status(),
            s.plan_id(),
            s.price_id(),
            s.current_period(),
            false,
            None,
            provider_time,
            now(),
        );
        assert!(s.is_subscription_event_stale(provider_time - chrono::Duration::minutes(1)));
        assert!(!s.is_subscription_event_stale(provider_time + chrono::Duration::minutes(1)));
    }

    #[test]
    fn provider_ordering_is_independent_of_local_processing_time() {
        let mut s = sub(SubscriptionStatus::Active, CollectionState::Ok);
        let first_provider_time = now() - chrono::Duration::hours(2);
        s.apply_stripe_update(
            s.status(),
            s.plan_id(),
            s.price_id(),
            s.current_period(),
            false,
            None,
            first_provider_time,
            now(),
        );

        assert!(!s.is_subscription_event_stale(first_provider_time + chrono::Duration::minutes(1)));
    }

    #[test]
    fn invoice_events_have_an_independent_ordering_cursor() {
        let mut s = sub(SubscriptionStatus::Active, CollectionState::Ok);
        let paid_at = now();
        s.record_invoice_event(paid_at, paid_at);

        assert!(s.is_invoice_event_stale(paid_at - chrono::Duration::seconds(1)));
        assert!(!s.is_invoice_event_stale(paid_at + chrono::Duration::seconds(1)));
        assert_eq!(s.stripe_subscription_event_at(), None);
    }
}
