//! Checkout Session creation: the only way a tenant starts paying.
//!
//! Flow: resolve the price → plan → ensure the tenant's Stripe customer
//! (idempotency-keyed) → tax gate → create the session (never with
//! `payment_method_types` — dynamic payment methods maximise conversion) →
//! return the hosted URL. Access is granted only when webhooks arrive, never
//! from the redirect.

use std::sync::Arc;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_persistence::TxManager;
use serde::Serialize;
use tracing::info;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::storage;
use crate::domain::{
    BillingError, CheckoutSessionRequest, CheckoutState, CheckoutStateRepository, Clock,
    PlanPriceRepository, PlanRepository, PriceId, StripeClient, SubscriptionRepository,
    SubscriptionStatus, TaxGate,
};

const CHECKOUT_STATE_TTL_HOURS: i64 = 24;

/// Random lowercase-letter suffix for the Checkout `integration_identifier`
/// (Dashboard comparison across flows; 8 letters per the Stripe guideline).
/// Derived from UUIDv7 bytes — sufficient entropy for a label suffix.
fn random_suffix(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    let mut bytes = Uuid::now_v7().as_u128().to_le_bytes().to_vec();
    while bytes.len() < len {
        bytes.extend_from_slice(&Uuid::now_v7().as_u128().to_le_bytes());
    }
    bytes[..len]
        .iter()
        .map(|b| ALPHABET[(b % 26) as usize] as char)
        .collect()
}

pub struct CreateCheckoutSessionCommand {
    pub tenant_id: Uuid,
    pub price_id: PriceId,
    pub success_url: Option<String>,
    pub cancel_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckoutSessionView {
    pub session_id: String,
    pub url: String,
}

/// Live (access-granting or not-yet-decided) statuses block a second
/// Checkout: changes go through the Customer Portal.
fn blocks_new_checkout(status: SubscriptionStatus) -> bool {
    matches!(
        status,
        SubscriptionStatus::Trialing
            | SubscriptionStatus::Active
            | SubscriptionStatus::PastDue
            | SubscriptionStatus::Unpaid
            | SubscriptionStatus::Incomplete
            | SubscriptionStatus::Paused
    )
}

pub struct CreateCheckoutSession {
    tx_manager: TxManager,
    plans: Arc<dyn PlanRepository>,
    prices: Arc<dyn PlanPriceRepository>,
    subscriptions: Arc<dyn SubscriptionRepository>,
    checkout_states: Arc<dyn CheckoutStateRepository>,
    stripe: Arc<dyn StripeClient>,
    tax_gate: Arc<dyn TaxGate>,
    audit: Arc<dyn AuditWriter>,
    clock: Arc<dyn Clock>,
    success_url: String,
    cancel_url: String,
    automatic_tax_enabled: bool,
    integration_identifier_suffix_len: usize,
}

#[allow(clippy::too_many_arguments)]
impl CreateCheckoutSession {
    pub fn new(
        tx_manager: TxManager,
        plans: Arc<dyn PlanRepository>,
        prices: Arc<dyn PlanPriceRepository>,
        subscriptions: Arc<dyn SubscriptionRepository>,
        checkout_states: Arc<dyn CheckoutStateRepository>,
        stripe: Arc<dyn StripeClient>,
        tax_gate: Arc<dyn TaxGate>,
        audit: Arc<dyn AuditWriter>,
        clock: Arc<dyn Clock>,
        success_url: String,
        cancel_url: String,
        automatic_tax_enabled: bool,
        integration_identifier_suffix_len: usize,
    ) -> Self {
        Self {
            tx_manager,
            plans,
            prices,
            subscriptions,
            checkout_states,
            stripe,
            tax_gate,
            audit,
            clock,
            success_url,
            cancel_url,
            automatic_tax_enabled,
            integration_identifier_suffix_len,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        cmd: CreateCheckoutSessionCommand,
    ) -> Result<CheckoutSessionView, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("create_checkout_session", "tenant")
            .await
            .map_err(storage)?;

        // Existing live subscription → the Customer Portal is the change
        // path, not a second Checkout.
        let existing = self
            .subscriptions
            .find_by_tenant_platform(&mut scope, cmd.tenant_id)
            .await?;
        if existing
            .as_ref()
            .is_some_and(|subscription| blocks_new_checkout(subscription.status()))
        {
            return Err(BillingError::SubscriptionExists);
        }
        // A terminal subscription means the prior Checkout is no longer a
        // valid route back to paid access, even when it was created recently.
        let force_new_checkout = existing.is_some();

        let now = self.clock.now();
        let proposed = CheckoutState {
            tenant_id: cmd.tenant_id,
            price_id: cmd.price_id,
            command_id: Uuid::now_v7(),
            stripe_customer_id: existing
                .as_ref()
                .and_then(|subscription| subscription.stripe_customer_id())
                .map(str::to_string),
            stripe_session_id: None,
            session_url: None,
            success_url: cmd.success_url.unwrap_or_else(|| self.success_url.clone()),
            cancel_url: cmd.cancel_url.unwrap_or_else(|| self.cancel_url.clone()),
            integration_identifier_suffix: random_suffix(self.integration_identifier_suffix_len),
            expires_at: now + chrono::Duration::hours(CHECKOUT_STATE_TTL_HOURS),
        };
        let checkout = self
            .checkout_states
            .claim(&mut scope, &proposed, now, force_new_checkout)
            .await?;
        let price = self
            .prices
            .find_by_id(&mut scope, checkout.price_id)
            .await?
            .ok_or_else(|| BillingError::PriceNotFound(checkout.price_id.to_string()))?;
        if !price.active() {
            return Err(BillingError::PriceNotActive(
                price.stripe_price_id().to_string(),
            ));
        }
        let plan = self
            .plans
            .find_by_id(&mut scope, price.plan_id())
            .await?
            .ok_or_else(|| BillingError::PlanNotFound(price.plan_id().to_string()))?;

        scope.commit().await.map_err(storage)?;

        if let (Some(session_id), Some(url)) = (
            checkout.stripe_session_id.clone(),
            checkout.session_url.clone(),
        ) {
            return Ok(CheckoutSessionView { session_id, url });
        }

        // The tax gate runs before the session exists: enabling
        // automatic_tax without an active registration silently collects
        // nothing — we fail loudly instead.
        let automatic_tax = if self.automatic_tax_enabled {
            if !self.tax_gate.has_active_registration().await? {
                return Err(BillingError::TaxRegistrationMissing);
            }
            true
        } else {
            false
        };

        let customer_id = match checkout.stripe_customer_id.clone() {
            Some(customer_id) => customer_id,
            None => {
                let customer = self
                    .stripe
                    .ensure_customer(cmd.tenant_id, checkout.command_id)
                    .await?;
                let mut scope = self
                    .tx_manager
                    .begin_unscoped("store_checkout_customer", "tenant")
                    .await
                    .map_err(storage)?;
                self.checkout_states
                    .set_customer(&mut scope, cmd.tenant_id, checkout.command_id, &customer.id)
                    .await?;
                scope.commit().await.map_err(storage)?;
                customer.id
            }
        };
        let session = self
            .stripe
            .create_checkout_session(&CheckoutSessionRequest {
                command_id: checkout.command_id,
                customer_id,
                stripe_price_id: price.stripe_price_id().to_string(),
                trial_period_days: price.trial_days(),
                tenant_id: cmd.tenant_id,
                success_url: checkout.success_url.clone(),
                cancel_url: checkout.cancel_url.clone(),
                automatic_tax,
                tax_id_collection: true,
                integration_identifier_suffix: checkout.integration_identifier_suffix.clone(),
            })
            .await?;

        // Audit the initiation (the webhook flow owns the outcome).
        let mut scope = self
            .tx_manager
            .begin_unscoped("audit_checkout_session", "tenant")
            .await
            .map_err(storage)?;
        self.checkout_states
            .complete(
                &mut scope,
                cmd.tenant_id,
                checkout.command_id,
                &session.session_id,
                &session.url,
            )
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::User,
                    actor_id: Some(actor),
                    on_behalf_of: None,
                    tenant_id: Some(cmd.tenant_id),
                    action: "billing.checkout_session_created".to_string(),
                    resource_type: Some("checkout_session".to_string()),
                    resource_id: Some(session.session_id.clone()),
                    before: None,
                    after: Some(serde_json::json!({
                        "plan": plan.key(),
                        "price": price.stripe_price_id(),
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| BillingError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;

        info!(
            tenant_id = %cmd.tenant_id,
            plan = plan.key(),
            session = %session.session_id,
            "checkout session created",
        );
        Ok(CheckoutSessionView {
            session_id: session.session_id,
            url: session.url,
        })
    }
}
