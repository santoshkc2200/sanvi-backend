//! Domain ports: the interfaces adapters implement. Stripe specifics live
//! only in adapters (sub-phase 09.2); the proof the abstraction holds is
//! the fake provider (09.1). Ports speak domain types only — no provider
//! SDK type may appear in a signature, enforced by
//! `scripts/check-layering.sh` (NFR-906).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use sanvi_persistence::{PlatformScope, ScopedTx};

use super::capability::{CapabilitySet, RequirementSummary};
use super::checkout::Checkout;
use super::connection::ProviderConnection;
use super::error::PaymentsError;
use super::money::{Currency, Money};
use super::payment::{PaymentRecord, PaymentStatus, RefundRecord, RefundStatus};
use super::provider::{ConnectionStatus, CountryCode, ProviderKind};

/// Persistence for the provider-connection aggregate. All access is
/// tenant-scoped (`ScopedTx` sets `sanvi.tenant_id`, so RLS enforces
/// isolation even if a query forgets the predicate).
#[async_trait]
pub trait ProviderConnectionRepository: Send + Sync {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError>;

    /// The tenant's currently-connected row for a provider, if any
    /// (`disconnected_at IS NULL`).
    async fn find_active(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        provider: &str,
    ) -> Result<Option<ProviderConnection>, PaymentsError>;

    /// Every connection the tenant has ever had, most recent first
    /// (history is retained for the statutory period, phase 05 override).
    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<ProviderConnection>, PaymentsError>;

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<ProviderConnection>, PaymentsError>;

    /// Persist an aggregate's current state (status transitions stamp
    /// `connected_at`/`disconnected_at` and refresh capabilities).
    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError>;
}

// ---------------------------------------------------------------------------
// The provider port: everything Sanvi asks a payment provider to do.
// ---------------------------------------------------------------------------

/// Static catalog metadata one adapter contributes to the provider
/// registry. The catalog endpoint is *derived* from these — a provider
/// appears in the settings page by being registered, never by being
/// hardcoded in a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub kind: ProviderKind,
    /// Human-readable name for cards and menus ("Stripe").
    pub display_name: String,
    /// Countries whose tenants may connect this provider; drives the
    /// per-tenant `available` flag in the catalog.
    pub supported_countries: Vec<CountryCode>,
    /// Whether connecting requires an onboarding flow.
    pub requires_onboarding: bool,
    /// `false` for the fake provider: the descriptor is only registered
    /// and listed in non-production environments.
    pub production: bool,
}

/// A request to create an account at the provider for a tenant. Carries
/// only connection-relevant data — onboarding PII is collected by the
/// provider's own embedded surfaces, never proxied through Sanvi.
#[derive(Debug, Clone)]
pub struct CreateAccountRequest {
    pub tenant_id: Uuid,
    pub country: Option<CountryCode>,
    pub default_currency: Option<Currency>,
    /// Business display name prefilled from tenant data (the tenant's
    /// display name — never user PII).
    pub display_name: Option<String>,
    /// The connection id we will persist this account under, sent as the
    /// provider-side idempotency key: a retry after a crash between
    /// creation and persistence adopts the same account instead of
    /// creating a second one.
    pub idempotency_key: Uuid,
}

/// The provider-side result of account creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccount {
    pub external_account_id: String,
}

/// An ephemeral session credential for the provider's embedded onboarding
/// component (Stripe Account Session client secrets — but said
/// provider-agnostically).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountSession {
    pub external_account_id: String,
    pub client_secret: String,
    pub expires_at: DateTime<Utc>,
}

/// The authoritative account state as of a fetch/sync. `status_hint` is
/// the provider's own opinion; the domain's
/// [`ProviderConnection::apply_status`](super::connection::ProviderConnection::apply_status)
/// remains the sole authority on what our stored status becomes.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAccountStatus {
    pub external_account_id: String,
    pub capabilities: CapabilitySet,
    pub requirements: RequirementSummary,
    pub country: Option<CountryCode>,
    pub default_currency: Option<Currency>,
    /// The status the provider's raw state maps to, if it maps to one.
    pub status_hint: Option<ConnectionStatus>,
}

/// A checkout session request on the connected account (direct charge —
/// the money is the tenant's).
#[derive(Debug, Clone)]
pub struct CreateCheckoutCommand {
    pub external_account_id: String,
    pub checkout: Checkout,
    /// Where the provider sends the customer on success/cancel — the
    /// tenant's own domain (phase 08). Hints only; truth arrives by
    /// webhook.
    pub success_url: String,
    pub cancel_url: String,
    pub locale: String,
    /// The platform's fee on this payment, when a fee policy is enabled
    /// (off in 0.10.0). Rides `payment_intent_data.application_fee_amount`
    /// — the only correct mechanism for direct charges, and never sent
    /// alongside transfers (there are none in this charge pattern).
    pub application_fee: Option<Money>,
}

/// The provider-side checkout session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCheckoutSession {
    pub external_session_id: String,
    pub url: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A refund request against a provider payment. The amount has already
/// passed the domain portion guard (≤ total, same currency).
#[derive(Debug, Clone)]
pub struct ProviderRefundCommand {
    pub external_account_id: String,
    pub external_payment_id: String,
    pub refund: RefundRecord,
    pub idempotency_key: Uuid,
}

/// The provider-side refund result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRefund {
    pub external_refund_id: String,
    pub status: RefundStatus,
    pub amount: Money,
}

/// The one port every payments slice speaks. Implementations: the fake
/// provider (tests, non-production environments) and, from 09.2, the
/// Stripe Connect adapter — nothing else may know a provider exists.
#[async_trait]
pub trait PaymentProviderPort: Send + Sync {
    /// The provider this adapter serves — also its registry key.
    fn kind(&self) -> ProviderKind;

    /// Catalog metadata for the registry.
    fn descriptor(&self) -> ProviderDescriptor;

    async fn create_account(
        &self,
        request: &CreateAccountRequest,
    ) -> Result<ProviderAccount, PaymentsError>;

    async fn create_account_session(
        &self,
        external_account_id: &str,
    ) -> Result<ProviderAccountSession, PaymentsError>;

    /// The provider's current account state (webhook handlers ask this
    /// when an event implies a change).
    async fn fetch_status(
        &self,
        external_account_id: &str,
    ) -> Result<ProviderAccountStatus, PaymentsError>;

    /// Full resync of the account state (the periodic job, 09.3). Same
    /// shape as [`PaymentProviderPort::fetch_status`]; the distinction is
    /// the caller's cadence, kept explicit so adapters may batch deeper
    /// queries on sync than on event-driven fetches.
    async fn sync(&self, external_account_id: &str)
    -> Result<ProviderAccountStatus, PaymentsError>;

    async fn create_checkout(
        &self,
        command: &CreateCheckoutCommand,
    ) -> Result<ProviderCheckoutSession, PaymentsError>;

    /// Re-fetch a live checkout session by its provider id — the
    /// idempotent-replay path returns the same hosted URL without
    /// creating a second session. Default: not shipped by this adapter.
    async fn retrieve_checkout(
        &self,
        _external_account_id: &str,
        _external_session_id: &str,
    ) -> Result<ProviderCheckoutSession, PaymentsError> {
        Err(PaymentsError::ProviderUnavailable(
            "checkout retrieval is not supported by this adapter".to_string(),
        ))
    }

    async fn refund(
        &self,
        command: &ProviderRefundCommand,
    ) -> Result<ProviderRefund, PaymentsError>;

    /// The connected-account payout history, newest first (read-only —
    /// payout *management* stays in the provider's own dashboard).
    async fn list_payouts(
        &self,
        external_account_id: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<ProviderPayout>, PaymentsError>;

    /// Verify a provider webhook signature against this adapter's
    /// configured secret (wrong-endpoint secrets must fail here — see
    /// [`crate::domain::webhook`]).
    fn verify_webhook(&self, payload: &[u8], signature_header: &str) -> Result<(), PaymentsError>;
}

/// One payout from the tenant's connected account to their bank. A read
/// model over provider data; Sanvi never moves this money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPayout {
    pub external_payout_id: String,
    pub amount: Money,
    pub status: PayoutStatus,
    pub arrival_at: Option<DateTime<Utc>>,
}

/// The payout states the read model distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutStatus {
    Pending,
    Paid,
    Failed,
}

impl PayoutStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Paid => "paid",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" => Self::Pending,
            "paid" => Self::Paid,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// Payout visibility for the tenant's payments dashboard (09.6): a
/// read-only projection, scoped to one connected account.
#[async_trait]
pub trait PayoutReadPort: Send + Sync {
    async fn list_payouts(
        &self,
        external_account_id: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<ProviderPayout>, PaymentsError>;
}

/// Webhook verification as a port: the transport (09.3) hands over raw
/// bytes plus the signature header and learns only pass/fail. The secret
/// is adapter state, never a domain concept.
#[async_trait]
pub trait WebhookVerifierPort: Send + Sync {
    async fn verify(&self, payload: &[u8], signature_header: &str) -> Result<(), PaymentsError>;
}

// ---------------------------------------------------------------------------
// 09.3: the webhook transport's persistence and lifecycle ports.
// ---------------------------------------------------------------------------

/// Storage for raw Connect webhook events (`payments.webhook_events`).
///
/// The table is **platform-scoped** (deliberately outside tenant RLS —
/// events arrive before the tenant is known and are routed by account id),
/// so every method takes a `PlatformScope`. The primary key on the
/// provider's event id *is* the dedupe: `insert_ignore` returns `false`
/// for a redelivery, which still answers Stripe 2xx.
#[async_trait]
pub trait WebhookEventRepository: Send + Sync {
    /// Store-first. `Ok(true)` when newly stored; `Ok(false)` for a
    /// redelivery of a known event id (a no-op, not an error).
    async fn insert_ignore(
        &self,
        scope: &mut PlatformScope<'_>,
        record: &super::webhook::WebhookEventRecord,
    ) -> Result<bool, PaymentsError>;

    /// The work queue: stored but unprocessed, retry budget not exhausted,
    /// oldest first.
    async fn fetch_unprocessed(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<super::webhook::WebhookEventRecord>, PaymentsError>;

    async fn mark_processed(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
    ) -> Result<(), PaymentsError>;

    /// Record one failed attempt (the error text backs the DLQ alert and
    /// the CLI replay view). Exhausting the retry budget dead-letters the
    /// event — it stops being fetched, so a poison event never blocks the
    /// queue.
    async fn mark_attempt_failed(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
        error: &str,
    ) -> Result<(), PaymentsError>;

    async fn unprocessed_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, PaymentsError>;

    async fn dead_letter_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, PaymentsError>;

    /// Dead-lettered events, oldest first (the CLI's replay triage list).
    async fn dead_letters(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<super::webhook::WebhookEventRecord>, PaymentsError>;

    /// Requeue one dead-lettered event after the root cause is fixed:
    /// clears the attempt budget so the dispatcher picks it up again.
    /// `Ok(false)` when the event is not dead-lettered (nothing to do).
    async fn replay(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
    ) -> Result<bool, PaymentsError>;
}

/// Platform-scope access to connection rows for the webhook dispatcher and
/// the reconciliation job — cross-tenant maintenance that runs under the
/// audited `PlatformScope` escape hatch of the shared RLS policy. Tenant
/// request paths keep using [`ProviderConnectionRepository`]; this port
/// exists so worker code cannot accidentally take a tenant-scoped
/// transaction around a loop of tenants.
#[async_trait]
pub trait ConnectionLifecycleStore: Send + Sync {
    /// The live connection owning this provider account, if any. Unknown
    /// accounts resolve to `None` — recorded and alerted, never provisioned.
    async fn find_live_by_external_account(
        &self,
        scope: &mut PlatformScope<'_>,
        external_account_id: &str,
    ) -> Result<Option<ProviderConnection>, PaymentsError>;

    /// Live connections not synced since `stale_before`, oldest first —
    /// the reconciliation job's candidates ("not seen recently").
    async fn list_due_for_sync(
        &self,
        scope: &mut PlatformScope<'_>,
        stale_before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ProviderConnection>, PaymentsError>;

    /// Persist an aggregate updated by the dispatcher/sync job.
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError>;

    /// Live connections per status (the status gauge).
    async fn count_by_status(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<(String, i64)>, PaymentsError>;
}

/// Tenant-admin notifications on connection-lifecycle changes, through the
/// phase-05/06 notification path (locale-resolved rendering in the worker;
/// a logging implementation everywhere else). Failures are logged by the
/// caller — a lost notification must never fail a committed transition.
#[async_trait]
pub trait ConnectionNotifier: Send + Sync {
    async fn connection_activated(
        &self,
        tenant_id: Uuid,
        connection_id: Uuid,
    ) -> Result<(), PaymentsError>;

    async fn connection_restricted(
        &self,
        tenant_id: Uuid,
        connection_id: Uuid,
        detail: &str,
    ) -> Result<(), PaymentsError>;

    /// A dispute was opened on one of the tenant's payments — the
    /// response deadline is attached when the provider supplies one.
    async fn dispute_opened(
        &self,
        tenant_id: Uuid,
        dispute_id: Uuid,
        due_by: Option<DateTime<Utc>>,
    ) -> Result<(), PaymentsError>;
    /// An outstanding requirement's deadline is approaching while the
    /// connection still cannot take payments.
    async fn requirement_deadline_approaching(
        &self,
        tenant_id: Uuid,
        connection_id: Uuid,
        deadline: DateTime<Utc>,
    ) -> Result<(), PaymentsError>;
}

// ---------------------------------------------------------------------------
// 09.4: checkout persistence, the phase-08 domain allow-list and the
// checkout throttle.
// ---------------------------------------------------------------------------

/// Tenant-scoped persistence for the checkout aggregate. Every access is
/// RLS-scoped like [`ProviderConnectionRepository`]: a checkout id
/// another tenant sends resolves to `None`, never to someone else's row.
#[async_trait]
pub trait CheckoutRepository: Send + Sync {
    /// Persist a fresh checkout (the create use case writes it *before*
    /// calling the provider, so an interrupted request is recoverable
    /// and the idempotency key has an owner).
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        checkout: &Checkout,
    ) -> Result<(), PaymentsError>;

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<Checkout>, PaymentsError>;

    /// The idempotency key's owner, if any (`checkouts_idempotency_idx`).
    async fn find_by_idempotency_key(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        key: Uuid,
    ) -> Result<Option<Checkout>, PaymentsError>;

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        checkout: &Checkout,
    ) -> Result<(), PaymentsError>;

    /// The newest payment recorded against this checkout, if any — the
    /// confirmation view derives its public status from it.
    async fn latest_payment_for_checkout(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        checkout_id: Uuid,
    ) -> Result<Option<super::payment::PaymentRecord>, PaymentsError>;
}

/// Platform-scope access for the webhook dispatcher — payment events
/// arrive cross-tenant and route by provider object id, exactly like the
/// connection lifecycle store above. Tenant request paths never touch
/// this port.
#[async_trait]
pub trait CheckoutLifecycleStore: Send + Sync {
    /// The checkout owning this provider session (checkout.session.*).
    async fn find_by_external_session(
        &self,
        scope: &mut PlatformScope<'_>,
        external_session_id: &str,
    ) -> Result<Option<Checkout>, PaymentsError>;

    /// The checkout named by event metadata (payment_intent.*) — scoped
    /// by the tenant id the metadata carries so one tenant's webhook can
    /// never move another tenant's row even with a forged metadata value.
    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        checkout_id: Uuid,
    ) -> Result<Option<Checkout>, PaymentsError>;

    /// Persist an aggregate updated by a webhook handler.
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        checkout: &Checkout,
    ) -> Result<(), PaymentsError>;

    /// Insert-or-update by `(tenant_id, external_payment_id)` — the
    /// unique index is the dedupe that makes every handler idempotent.
    async fn upsert_payment(
        &self,
        scope: &mut PlatformScope<'_>,
        payment: &super::payment::PaymentRecord,
    ) -> Result<(), PaymentsError>;

    /// An existing payment record for this provider id, if we wrote one
    /// before (redelivery / sibling-event ordering).
    async fn find_payment_by_external_id(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        external_payment_id: &str,
    ) -> Result<Option<super::payment::PaymentRecord>, PaymentsError>;
}

/// The phase-08 verified-domain allow-list. A checkout's success/cancel
/// URLs are accepted only when their host is a live domain *of this
/// tenant*; implemented against the domains context's live-hostname
/// projection in the composition root. A port, not a query: the payments
/// context stays decoupled from how phase 08 stores its truth.
#[async_trait]
pub trait TenantDomainAllowList: Send + Sync {
    async fn allows(&self, tenant_id: Uuid, hostname: &str) -> Result<bool, PaymentsError>;
}

/// The checkout throttle. Session-creation flooding is Sanvi's problem
/// (card-testing at the hosted page is the tenant's): buckets per
/// tenant, per client IP and per customer reference, checked before any
/// database or provider work. Implemented over Redis in production;
/// an in-process fixed window ships for tests and degraded mode.
#[async_trait]
pub trait CheckoutThrottle: Send + Sync {
    /// Consume one token from the named bucket. `Err(RateLimited)`
    /// denies; any other error fails closed.
    async fn acquire(&self, bucket: &str) -> Result<(), PaymentsError>;
}

// ---------------------------------------------------------------------------
// 09.5: payments, refunds & disputes read models.
// ---------------------------------------------------------------------------

/// How payments are sorted for list queries. Currently only created-at
/// ordering exists — the spec's cursor is (created_at, id) and the UI's
/// table defaults to newest-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaymentSort {
    #[default]
    CreatedAtDesc,
}

/// Filter set for `GET /api/v1/tenant/payments` — every documented
/// filter combinable, cursor-paginated.
#[derive(Debug, Clone, Default)]
pub struct PaymentFilter {
    /// Exact status match (`succeeded`, `failed`, …) — absent = any.
    pub status: Option<super::payment::PaymentStatus>,
    /// Exact currency match (ISO-4217) — absent = any.
    pub currency: Option<Currency>,
    /// Customer reference substring — the tenant's own order/customer
    /// identifier as stored in the checkout's `customer_ref` JSON. Absent
    /// = any. Implementation may treat this as a substring search over
    /// the JSON text, never as an exact structured match (free-form).
    pub customer: Option<String>,
    /// Inclusive date range over the payment's creation time (UTC) —
    /// absent bounds are unbounded. The range remains the tenant's
    /// timezone-preferred view's naive UTC translation; the stored column
    /// is UTC.
    pub date_from: Option<DateTime<Utc>>,
    pub date_to: Option<DateTime<Utc>>,
    /// Cursor for pagination: opaque string encoding the last row's
    /// `(created_at, id)` tuple, as returned by the previous page's
    /// `next_cursor`. The first page passes `None`.
    pub cursor: Option<String>,
    /// Requested page size. Capped server-side (max 100).
    pub limit: Option<u32>,
    /// Sort order — only `CreatedAtDesc` in this release.
    pub sort: PaymentSort,
}

/// One page of payments plus the cursor for the next one, when more
/// rows exist.
#[derive(Debug, Clone, PartialEq)]
pub struct PaymentPage {
    pub items: Vec<super::payment::PaymentRecord>,
    pub next_cursor: Option<String>,
}

/// Tenant-scoped read of the payment projection. The projection is kept
/// by 09.4/09.5 webhooks (never fetched from Stripe per request); this
/// port reads the tenant's own rows via RLS-scoped transactions.
#[async_trait]
pub trait PaymentRepository: Send + Sync {
    /// One payment by its Sanvi id, tenant-scoped (404 outside the tenant).
    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<super::payment::PaymentRecord>, PaymentsError>;

    /// Locate by the provider's external id (`pi_…` / `pay_…`), tenant-
    /// scoped — the webhook payment handlers need this, but the read
    /// path also uses it for dispute routing.
    async fn find_by_external_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        external_payment_id: &str,
    ) -> Result<Option<super::payment::PaymentRecord>, PaymentsError>;

    /// Platform-scope lookup for webhook routing (charge objects name
    /// charges, not payment intents — the webhook dispatcher needs a
    /// payment row for that charge id).
    async fn find_by_external_id_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        external_payment_id: &str,
    ) -> Result<Option<super::payment::PaymentRecord>, PaymentsError>;

    /// The list view — every documented filter combinable, cursor-
    /// paginated, newest-first.
    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        filter: &PaymentFilter,
    ) -> Result<PaymentPage, PaymentsError>;

    /// All payments for CSV export that match `filter` (same predicates
    /// as `list` but streamed). The caller owns auditing and the
    /// streaming response.
    async fn list_for_export(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        filter: &PaymentFilter,
    ) -> Result<Vec<super::payment::PaymentRecord>, PaymentsError>;
}

/// Filter for `GET /api/v1/tenant/payments/disputes`.
#[derive(Debug, Clone, Default)]
pub struct DisputeFilter {
    pub status: Option<super::dispute::DisputeStatus>,
    pub currency: Option<Currency>,
    pub date_from: Option<DateTime<Utc>>,
    pub date_to: Option<DateTime<Utc>>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// One page of disputes plus the cursor for the next.
#[derive(Debug, Clone, PartialEq)]
pub struct DisputePage {
    pub items: Vec<super::dispute::DisputeRecord>,
    pub next_cursor: Option<String>,
}

/// Tenant-scoped read of the dispute projection.
#[async_trait]
pub trait DisputeRepository: Send + Sync {
    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<super::dispute::DisputeRecord>, PaymentsError>;

    /// Locate by the provider's external dispute id (`dp_…`), platform
    /// scope — disputes arrive via `charge.dispute.*` webhooks before the
    /// tenant is known beyond the account routing.
    async fn find_by_external_id_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        external_dispute_id: &str,
    ) -> Result<Option<super::dispute::DisputeRecord>, PaymentsError>;

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        filter: &DisputeFilter,
    ) -> Result<DisputePage, PaymentsError>;

    async fn list_for_payment(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        payment_id: Uuid,
    ) -> Result<Vec<super::dispute::DisputeRecord>, PaymentsError>;

    /// Insert-or-update by `(tenant_id, external_dispute_id)` — the
    /// unique index is the dedupe that makes every handler idempotent.
    async fn upsert(
        &self,
        scope: &mut PlatformScope<'_>,
        dispute: &super::dispute::DisputeRecord,
    ) -> Result<(), PaymentsError>;
}

/// Tenant-scoped read/write of refunds.
#[async_trait]
pub trait RefundRepository: Send + Sync {
    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<super::payment::RefundRecord>, PaymentsError>;

    async fn find_by_idempotency_key(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        key: Uuid,
    ) -> Result<Option<super::payment::RefundRecord>, PaymentsError>;

    async fn find_by_external_id_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        external_refund_id: &str,
    ) -> Result<Option<super::payment::RefundRecord>, PaymentsError>;

    async fn list_for_payment(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        payment_id: Uuid,
    ) -> Result<Vec<super::payment::RefundRecord>, PaymentsError>;

    /// The sum of `succeeded` refunds for a payment (minor units, same
    /// currency as the payment). Returns zero when no succeeded refund
    /// exists. Used for the over-refund guard.
    async fn sum_succeeded_for_payment(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        payment_id: Uuid,
    ) -> Result<i64, PaymentsError>;

    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        refund: &super::payment::RefundRecord,
    ) -> Result<(), PaymentsError>;

    /// Tenant-scoped lookup by the provider's refund id — the finalize
    /// path checks it to merge with a webhook-created row when the
    /// webhook won the race (`refunds_external_idx`).
    async fn find_by_external_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        external_refund_id: &str,
    ) -> Result<Option<super::payment::RefundRecord>, PaymentsError>;

    /// Record the provider's answer: external id + status, only when the
    /// row has not already succeeded. Returns whether the row changed; the
    /// caller emits `RefundIssued` only when the provider status succeeded.
    async fn try_finalize(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        refund: &super::payment::RefundRecord,
    ) -> Result<bool, PaymentsError>;

    /// Mark a row failed after the provider rejected the refund, so the
    /// read model never shows a pending refund that does not exist at
    /// the provider and the key can be re-driven on retry.
    async fn mark_failed(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        refund_id: Uuid,
    ) -> Result<(), PaymentsError>;

    /// Delete a superseded local row (the webhook path won the race and
    /// already owns the provider refund id).
    async fn delete_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        refund_id: Uuid,
    ) -> Result<(), PaymentsError>;

    /// Attach an API idempotency key to a webhook-created winner after its
    /// superseded local row has been removed.
    async fn claim_idempotency_key(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        refund_id: Uuid,
        idempotency_key: Uuid,
    ) -> Result<(), PaymentsError>;

    /// Insert-or-update by `(tenant_id, external_refund_id)` for webhook
    /// reconciliation (Dashboard refunds). Platform scope, same idempotency
    /// role as the checkout payment upsert.
    async fn upsert_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        refund: &super::payment::RefundRecord,
    ) -> Result<(), PaymentsError>;
}
