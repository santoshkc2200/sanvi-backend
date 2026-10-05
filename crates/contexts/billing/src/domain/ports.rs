//! Ports (traits) implemented by driven adapters.
//!
//! The scoping guard is structural: tenant-scoped ports take
//! `&mut ScopedTx`, platform-wide ports take `&mut PlatformScope` — both are
//! capability tokens only constructible through `TxManager::begin_for` /
//! `begin_unscoped`, so a repository method that forgets its scope is a type
//! error, not a row leak.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::{PlatformScope, ScopedTx};
use uuid::Uuid;

use crate::domain::error::BillingError;
use crate::domain::events::BillingEvent;
use crate::domain::invoice::{InvoiceId, InvoiceRef, InvoiceStatus};
use crate::domain::plan::{EntitlementGrant, Plan, PlanId, PlanPrice, PriceId};
use crate::domain::subscription::{
    CollectionState, Period, Subscription, SubscriptionId, SubscriptionStatus,
};

/// The plan catalog (platform-scoped; plans are global, not per-tenant).
#[async_trait]
pub trait PlanRepository: Send + Sync {
    async fn save(&self, scope: &mut PlatformScope<'_>, plan: &Plan) -> Result<(), BillingError>;

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: PlanId,
    ) -> Result<Option<Plan>, BillingError>;

    async fn find_by_key(
        &self,
        scope: &mut PlatformScope<'_>,
        key: &str,
    ) -> Result<Option<Plan>, BillingError>;

    /// All plans, ordered by sort_order (visibility filter optional).
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        public_only: bool,
    ) -> Result<Vec<Plan>, BillingError>;
}

/// Prices per plan.
#[async_trait]
pub trait PlanPriceRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        price: &PlanPrice,
    ) -> Result<(), BillingError>;

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: PriceId,
    ) -> Result<Option<PlanPrice>, BillingError>;

    async fn find_by_stripe_price_id(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_price_id: &str,
    ) -> Result<Option<PlanPrice>, BillingError>;

    /// Active prices for a plan (public pricing page).
    async fn list_active_for_plan(
        &self,
        scope: &mut PlatformScope<'_>,
        plan_id: PlanId,
    ) -> Result<Vec<PlanPrice>, BillingError>;
}

/// The plan → entitlement map.
#[async_trait]
pub trait PlanEntitlementRepository: Send + Sync {
    /// Replace the whole map for a plan (atomic upsert semantics).
    async fn replace_for_plan(
        &self,
        scope: &mut PlatformScope<'_>,
        plan_id: PlanId,
        grants: &[EntitlementGrant],
    ) -> Result<(), BillingError>;

    async fn list_for_plan(
        &self,
        scope: &mut PlatformScope<'_>,
        plan_id: PlanId,
    ) -> Result<Vec<EntitlementGrant>, BillingError>;
}

/// Subscriptions. Tenant-scoped reads run under `ScopedTx` (RLS-filtered by
/// construction); webhook/reconciliation writes run under the audited
/// `PlatformScope`.
#[async_trait]
pub trait SubscriptionRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        subscription: &Subscription,
    ) -> Result<(), BillingError>;

    /// Phase-05 erasure: null the Stripe identifiers on the tenant's
    /// subscription projection (the row itself is an account/entitlement
    /// record, not personal data once the pointers are gone).
    async fn anonymise_stripe_ids(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<usize, BillingError>;

    /// Remove a tenant's projection (used when an operator override is
    /// revoked; Stripe-backed history is retained by status instead).
    async fn delete_by_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<bool, BillingError>;

    /// The tenant's own row (RLS sees only them).
    async fn find_by_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<Subscription>, BillingError>;

    /// The same row, read under the audited platform scope (checkout,
    /// webhook and reconciliation paths).
    async fn find_by_tenant_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<Subscription>, BillingError>;

    async fn find_by_stripe_subscription_id(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_subscription_id: &str,
    ) -> Result<Option<Subscription>, BillingError>;

    /// Stripe-backed rows with a live status (reconciliation re-reads these
    /// from Stripe; override rows are skipped).
    async fn list_reconcilable(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<Subscription>, BillingError>;

    /// Dunning rows whose grace window started before `deadline`
    /// (the dunning sweep).
    async fn list_dunning_due(
        &self,
        scope: &mut PlatformScope<'_>,
        deadline: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Subscription>, BillingError>;

    /// Trials ending soon that have not been notified (trial sweep).
    async fn list_trials_ending_soon(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        within: chrono::Duration,
        limit: u32,
    ) -> Result<Vec<Subscription>, BillingError>;

    /// Subscription counts per status (the status gauge source).
    async fn count_by_status(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<(SubscriptionStatus, i64)>, BillingError>;

    /// Subscriptions inside the dunning grace window (the dunning gauge).
    async fn dunning_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, BillingError>;

    /// Monthly recurring revenue grouped by currency. Annual prices are
    /// normalized to one month before aggregation.
    async fn mrr_by_currency(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<(String, f64)>, BillingError>;
}

/// Durable state for an in-flight Checkout Session. Keeping the Stripe
/// idempotency key and request inputs locally makes concurrent requests and
/// process retries converge on one Customer and one Session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutState {
    pub tenant_id: TenantId,
    pub price_id: PriceId,
    pub command_id: Uuid,
    pub stripe_customer_id: Option<String>,
    pub stripe_session_id: Option<String>,
    pub session_url: Option<String>,
    pub success_url: String,
    pub cancel_url: String,
    pub integration_identifier_suffix: String,
    pub expires_at: DateTime<Utc>,
}

#[async_trait]
pub trait CheckoutStateRepository: Send + Sync {
    /// Insert a new state, or replace an expired state while retaining its
    /// Stripe Customer. An unexpired state is returned unchanged.
    async fn claim(
        &self,
        scope: &mut PlatformScope<'_>,
        proposed: &CheckoutState,
        now: DateTime<Utc>,
        force_new: bool,
    ) -> Result<CheckoutState, BillingError>;

    async fn set_customer(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        command_id: Uuid,
        stripe_customer_id: &str,
    ) -> Result<(), BillingError>;

    async fn complete(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        command_id: Uuid,
        stripe_session_id: &str,
        session_url: &str,
    ) -> Result<(), BillingError>;

    async fn customer_for_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<String>, BillingError>;

    async fn anonymise(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<usize, BillingError>;
}

/// Invoices.
#[async_trait]
pub trait InvoiceRepository: Send + Sync {
    /// Upsert on stripe_invoice_id only when this event is not older than the
    /// projection's last Stripe event. Returns whether the row was applied.
    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        invoice: &InvoiceRef,
        event_created: DateTime<Utc>,
    ) -> Result<bool, BillingError>;

    /// The tenant's invoices, newest first (RLS sees only them), keyset-
    /// paginated on the Snowflake id: `after_id` is the id of the last
    /// invoice of the previous page, `None` for the first page.
    async fn list_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<InvoiceRef>, BillingError>;

    /// The same rows, read under the audited platform scope.
    async fn list_for_tenant_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<InvoiceRef>, BillingError>;

    async fn find_by_stripe_invoice_id(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_invoice_id: &str,
    ) -> Result<Option<InvoiceRef>, BillingError>;
}

/// One stored webhook event awaiting (or having completed) processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEventRecord {
    pub stripe_event_id: String,
    pub event_type: String,
    pub api_version: Option<String>,
    pub payload: serde_json::Value,
    pub created: DateTime<Utc>,
    /// When we stored it (the lag metric measures reception → processing).
    pub received_at: DateTime<Utc>,
    pub attempts: i32,
}

/// Store-first webhook persistence. The PK on `stripe_event_id` makes
/// redelivery a no-op.
#[async_trait]
pub trait WebhookEventRepository: Send + Sync {
    /// Insert; silently ignores duplicates (idempotent redelivery).
    async fn insert_ignore(
        &self,
        scope: &mut PlatformScope<'_>,
        record: &WebhookEventRecord,
    ) -> Result<bool, BillingError>;

    /// Phase-05 erasure: scrub every stored webhook payload that mentions
    /// this Stripe customer id (the raw payloads are the real local
    /// personal-data leak: they contain email, name and address).
    async fn scrub_payloads_for_customer(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_customer_id: &str,
    ) -> Result<usize, BillingError>;

    /// Unprocessed events (not yet `processed_at`, attempts < max), oldest
    /// first — the webhook dispatcher's work queue.
    async fn fetch_unprocessed(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<WebhookEventRecord>, BillingError>;

    async fn mark_processed(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
    ) -> Result<(), BillingError>;

    /// Record a failed attempt (attempts + 1, error message). Rows that
    /// exhaust retries become the dead-letter set the alert watches.
    async fn mark_attempt_failed(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
        error: &str,
    ) -> Result<(), BillingError>;

    /// How many events await processing (the alert + gauge source).
    async fn unprocessed_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, BillingError>;

    /// Events that exhausted their retry budget (the dead-letter gauge).
    async fn dead_letter_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, BillingError>;
}

/// A Stripe customer identity (created or reused for a tenant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeCustomer {
    pub id: String,
}

/// Request shape for a Checkout Session (subscription mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSessionRequest {
    /// Our command id — becomes the Stripe idempotency key.
    pub command_id: Uuid,
    pub customer_id: String,
    pub stripe_price_id: String,
    pub trial_period_days: Option<u16>,
    pub tenant_id: TenantId,
    pub success_url: String,
    pub cancel_url: String,
    /// Only set when the tax gate confirmed active registrations exist.
    pub automatic_tax: bool,
    /// Collect B2B tax ids (reverse-charge treatment).
    pub tax_id_collection: bool,
    /// Randomized suffix per the Dashboard-comparison requirement.
    pub integration_identifier_suffix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSessionResult {
    pub session_id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalSessionRequest {
    /// Our command id — becomes the Stripe idempotency key.
    pub command_id: Uuid,
    pub customer_id: String,
    pub return_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalSessionResult {
    pub url: String,
}

/// A subscription as read back from Stripe (reconciliation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeSubscriptionView {
    pub id: String,
    pub customer_id: String,
    pub status: SubscriptionStatus,
    pub stripe_price_id: Option<String>,
    pub current_period: Option<Period>,
    pub cancel_at_period_end: bool,
    pub trial_end: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
}

/// An active Stripe Tax registration (jurisdiction-level).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxRegistration {
    pub id: String,
    pub country: String,
}

/// The tax gate: `automatic_tax` may only be enabled on Checkout when at
/// least one active Stripe Tax registration exists (enabling it without one
/// silently collects nothing — the most common Stripe Tax mistake). The
/// adapter caches the answer with a short TTL.
#[async_trait]
pub trait TaxGate: Send + Sync {
    /// Whether at least one active registration exists.
    async fn has_active_registration(&self) -> Result<bool, BillingError>;
}

/// The Stripe gateway, implemented by the HTTP adapter (pinned API version,
/// idempotency keys, restricted key, retries). All calls fail closed: never
/// invent a "success" Stripe did not confirm.
#[async_trait]
pub trait StripeClient: Send + Sync {
    /// Create (or reuse, via idempotency) the tenant's Stripe customer.
    async fn ensure_customer(
        &self,
        tenant_id: TenantId,
        command_id: Uuid,
    ) -> Result<StripeCustomer, BillingError>;

    async fn create_checkout_session(
        &self,
        request: &CheckoutSessionRequest,
    ) -> Result<CheckoutSessionResult, BillingError>;

    async fn create_portal_session(
        &self,
        request: &PortalSessionRequest,
    ) -> Result<PortalSessionResult, BillingError>;

    /// Re-read a subscription from Stripe (the reconciliation job).
    async fn retrieve_subscription(
        &self,
        stripe_subscription_id: &str,
    ) -> Result<StripeSubscriptionView, BillingError>;

    /// Active Stripe Tax registrations (the tax gate's source of truth).
    async fn list_active_tax_registrations(&self) -> Result<Vec<TaxRegistration>, BillingError>;

    /// Phase-05 erasure: delete the Stripe customer (and its cards, sources
    /// and subscriptions). A missing customer is success — gone either way.
    async fn delete_customer(&self, stripe_customer_id: &str) -> Result<(), BillingError>;
}

/// The access context's entitlement administration, called by webhook
/// handlers, the dunning sweep and the platform override: replaces the
/// tenant's subscription-source rows with the plan's grant set. The adapter
/// owns the whole side effect set — it writes the rows through the access
/// context's repository *and* emits `billing.entitlements_recomputed` into
/// the same outbox transaction, so the worker invalidates entitlement caches
/// and no caller can forget either half.
#[async_trait]
pub trait EntitlementSyncer: Send + Sync {
    /// Replace subscription-source rows with `grants` (and emit
    /// EntitlementsRecomputed).
    async fn sync_subscription(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        plan_key: &str,
        grants: &[EntitlementGrant],
    ) -> Result<(), BillingError>;

    /// Remove every subscription-source row (downgrade to catalog defaults)
    /// and emit EntitlementsRecomputed.
    async fn clear_subscription(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<(), BillingError>;
}

/// The tenancy context's lifecycle, called by the dunning sweep: grace
/// expiry suspends the tenant with the billing reason; payment recovery
/// resumes it — but only when *we* suspended it (never overriding a legal or
/// abuse suspension).
#[async_trait]
pub trait TenantLifecycle: Send + Sync {
    /// Suspend for billing (idempotent: an already-suspended tenant stays
    /// as-is, whatever the reason).
    async fn suspend_billing(&self, tenant_id: TenantId) -> Result<(), BillingError>;

    /// Resume, but only from a billing suspension.
    async fn resume_billing(&self, tenant_id: TenantId) -> Result<(), BillingError>;
}

/// Publishes billing events into the transactional outbox.
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        events: &[BillingEvent],
    ) -> Result<(), BillingError>;
}

/// Source of new ids (tests substitute deterministic generators).
pub trait IdGenerator: Send + Sync {
    fn next_plan_id(&self) -> Result<PlanId, BillingError>;
    fn next_price_id(&self) -> Result<PriceId, BillingError>;
    fn next_subscription_id(&self) -> Result<SubscriptionId, BillingError>;
    fn next_invoice_id(&self) -> Result<InvoiceId, BillingError>;
}

/// The system clock (tests substitute fixed times).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Minimal invoice fields parsed from Stripe webhook payloads (the event
/// applier maps them into an [`InvoiceRef`]).
#[derive(Debug, Clone)]
pub struct StripeInvoiceData {
    pub id: String,
    pub number: Option<String>,
    pub status: InvoiceStatus,
    pub total_minor: i64,
    pub currency: String,
    pub hosted_url: Option<String>,
    pub pdf_url: Option<String>,
    pub period: Option<Period>,
    pub created: DateTime<Utc>,
    pub paid_at: Option<DateTime<Utc>>,
    pub subscription_id: Option<String>,
}

/// Minimal subscription fields parsed from Stripe webhook payloads.
#[derive(Debug, Clone)]
pub struct StripeSubscriptionData {
    pub id: String,
    pub customer_id: String,
    pub status: SubscriptionStatus,
    pub stripe_price_id: Option<String>,
    pub current_period: Option<Period>,
    pub cancel_at_period_end: bool,
    pub trial_end: Option<DateTime<Utc>>,
    pub metadata: serde_json::Value,
    pub created: DateTime<Utc>,
}

/// One invoice period helper shared by webhook/reconciliation mapping.
pub fn period_from_range(start: Option<i64>, end: Option<i64>) -> Option<Period> {
    match (start, end) {
        (Some(start), Some(end)) if end > start => Some(Period {
            start: DateTime::from_timestamp(start, 0)?,
            end: DateTime::from_timestamp(end, 0)?,
        }),
        _ => None,
    }
}

/// Convenience: build a `CollectionState` from a raw row value.
pub fn collection_state_parse_or_ok(raw: &str) -> CollectionState {
    CollectionState::parse(raw).unwrap_or(CollectionState::Ok)
}
