//! The Payments HTTP contract the platform publishes in `api/payments.yaml`.
//! The standalone Payments service produces and consumes these bodies; the
//! gateway relays them unchanged, so these types are never constructed here.
//! They must stay in step with the service's own definitions.

use chrono::{DateTime, Utc};
use sanvi_kernel::UserId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::domain::{CheckoutId, ConnectionId, DisputeId, PaymentId, RefundId};

/// One connectable provider as the settings page renders it. Derived
/// from the adapter's [`crate::domain::ports::ProviderDescriptor`] —
/// never hardcoded per provider anywhere in this layer or the handlers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProviderView {
    /// Stable provider identifier (`stripe_connect`, `fake`).
    #[schema(example = "stripe_connect")]
    pub kind: String,
    /// Whether *this tenant* may connect (country support).
    #[schema(example = true)]
    pub available: bool,
    /// Human-readable name for cards and menus.
    #[schema(example = "Stripe")]
    pub display_name: String,
    /// ISO-3166-1 alpha-2 countries whose tenants may connect.
    #[schema(example = json!(["US", "JP", "GB", "DE"]))]
    pub supported_countries: Vec<String>,
    /// Whether connecting requires an onboarding flow.
    #[schema(example = true)]
    pub requires_onboarding: bool,
}

/// `GET /api/v1/tenant/payments/providers` — the catalog envelope,
/// derived from the provider registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProvidersView {
    /// Connectable providers, in stable kind order.
    pub providers: Vec<ProviderView>,
}

/// One outstanding provider requirement, with the i18n message key the
/// frontend renders it from. The mapping is data
/// (`payments.requirement_keys`); a code the map has not learned falls
/// back to `payments.req.unmapped`, whose catalog entry shows the raw code
/// plus Stripe's requirements help link — new requirements degrade,
/// never break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RequirementEntryView {
    /// The provider's requirement code, verbatim.
    #[schema(example = "individual.verification.document")]
    pub code: String,
    /// The i18n key to render (`payments.req.*`).
    #[schema(example = "payments.req.id_document")]
    pub summary_key: String,
}

/// The connection's outstanding-requirements summary, projected with
/// render keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RequirementsView {
    pub currently_due: Vec<RequirementEntryView>,
    pub eventually_due: Vec<RequirementEntryView>,
    pub past_due: Vec<RequirementEntryView>,
    #[schema(value_type = Option<String>)]
    pub deadline: Option<DateTime<Utc>>,
}

/// One reason the connection cannot take payments yet, keyed for i18n.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct BlockerView {
    #[schema(example = "payments.blocker.card_payments_inactive")]
    pub summary_key: String,
}

/// A tenant's connection to a payment provider: identifiers and status
/// only — no PII, no credentials (the session secret is a separate,
/// never-persisted response).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[schema(as = PaymentsConnectionView)]
pub struct ConnectionView {
    #[schema(value_type = String)]
    pub id: ConnectionId,
    #[schema(example = "stripe_connect")]
    pub provider: String,
    #[schema(example = "onboarding")]
    pub status: String,
    /// The capability map as flat `name → status` strings
    /// (`{"card_payments": "active"}`), normalised from the provider's
    /// stored JSON (flat or v2-nested).
    pub capabilities: Value,
    /// Outstanding requirements with their i18n render keys and deadline.
    pub requirements: RequirementsView,
    /// Why checkout is blocked right now, if it is — server-computed,
    /// ordered, empty once active with nothing past due.
    pub blockers: Vec<BlockerView>,
    pub country: Option<String>,
    pub default_currency: Option<String>,
    #[schema(value_type = Option<String>)]
    pub connected_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Server-computed from the v2 `card_payments` capability — never a
    /// deprecated boolean. The frontend renders this; it never recomputes
    /// readiness from capability strings (one definition, one place).
    pub can_accept_payments: bool,
}

/// `POST /api/v1/tenant/payments/connections` — start (or resume) a
/// connection. Idempotent per `Idempotency-Key`: a retry returns the
/// same connection; a crash between provider-side creation and
/// persistence adopts the same account.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[schema(as = PaymentsCreateConnectionRequest)]
pub struct CreateConnectionRequest {
    /// Provider kind, as listed by the catalog.
    #[schema(example = "stripe_connect")]
    pub provider: String,
    /// ISO-3166-1 alpha-2 country the tenant trades from; defaults to the
    /// tenant's region.
    #[schema(example = "JP")]
    pub country: Option<String>,
    /// ISO-4217 default currency for the connected account.
    #[schema(example = "JPY")]
    pub default_currency: Option<String>,
}

/// A short-lived Account Session for the embedded onboarding component.
/// Fetched per render; never persisted, never logged, single-render.
#[derive(Clone, Serialize, ToSchema)]
pub struct ConnectionSessionView {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    #[schema(example = "stripe_connect")]
    pub provider: String,
    /// The provider's embedded-components client secret. Handle like a
    /// credential: no caching, no logging, one render.
    pub client_secret: String,
    #[schema(value_type = String)]
    pub expires_at: DateTime<Utc>,
    /// The embedded components enabled on this session:
    /// `account_onboarding`, `notification_banner`, and `account_management`.
    pub components: Vec<String>,
}

/// One reason a disconnect is refused: the stable code the API contract
/// fixes plus the i18n key the settings page renders it from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DisconnectBlockerView {
    #[schema(example = "in_flight_payments")]
    pub code: String,
    #[schema(example = "payments.blocker.in_flight_payments")]
    pub summary_key: String,
}

/// `POST /api/v1/tenant/checkout` — create a checkout session on the
/// tenant's connected account (09.4, direct charges). Idempotency-Key is
/// required: a double-clicked buy button must not create two sessions.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateCheckoutRequest {
    /// The merchant's order reference, echoed on every read model.
    #[schema(example = "order-2026-0821")]
    pub reference: String,
    /// Minor units — ¥2,000 is `2000` (JPY is zero-decimal).
    #[schema(example = 2000)]
    pub amount_minor: i64,
    /// ISO-4217 code; must match the connected account's settlement
    /// currency.
    #[schema(example = "JPY")]
    pub currency: String,
    /// Free-form customer/order context (provider-passthrough; no card
    /// data ever). Rate limiting buckets one key per distinct value.
    #[schema(value_type = Option<Object>)]
    pub customer_ref: Option<Value>,
    /// Presentation locale for the hosted page (`ja`, `en-US`, …).
    pub locale: Option<String>,
    /// Redirect target on a **verified** tenant domain (phase 08). A
    /// success URL on any other host is rejected outright.
    #[schema(example = "https://shop.tenant.test/thanks")]
    pub success_url: String,
    #[schema(example = "https://shop.tenant.test/cart")]
    pub cancel_url: String,
}

/// The checkout as the storefront sees it. `status` is the only source
/// the confirmation page trusts: derived from webhook-recorded truth,
/// never from the redirect.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckoutView {
    #[schema(value_type = String)]
    pub id: CheckoutId,
    /// `pending` | `paid` | `failed` | `expired` | `canceled`.
    #[schema(example = "pending")]
    pub status: String,
    pub reference: String,
    #[schema(example = 2000)]
    pub amount_minor: i64,
    #[schema(example = "JPY", value_type = String)]
    pub currency: String,
    #[schema(value_type = Option<Object>)]
    pub customer_ref: Option<Value>,
    /// The provider-hosted payment URL, while the checkout can still be
    /// paid. Absent once terminal.
    #[schema(example = "https://checkout.stripe.com/c/pay/cs_…", value_type = Option<String>)]
    pub url: Option<String>,
    #[schema(value_type = Option<String>)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Server-issued exactly once per order when paid; identical on every
    /// refresh of the confirmation page, so phase 10's conversion
    /// pipeline cannot double-count.
    /// The provider's decline/failure code for the newest failed attempt
    /// (`card_declined`, `insufficient_funds`, …), so the storefront can
    /// tell a buyer *why*. The provider's own message is deliberately not
    /// exposed: the storefront renders its own localized copy.
    #[schema(example = "card_declined")]
    pub failure_code: Option<String>,
    #[schema(value_type = Option<String>)]
    pub conversion_event_id: Option<PaymentId>,
    #[schema(value_type = String)]
    pub created_at: DateTime<Utc>,
}

/// What the storefront needs before it can render a Pay button:
/// readiness and the settlement currency. Derived from the tenant's live
/// connection by the same gate the checkout itself applies.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckoutConfigView {
    /// Server-computed from the v2 `card_payments` capability, exactly as
    /// `POST /tenant/checkout` computes it. The storefront renders this;
    /// it never recomputes readiness of its own.
    pub can_accept_payments: bool,
    /// The i18n key for *why* not (`payments.blocker.*`), so the
    /// storefront can show a reason next to a disabled button instead of
    /// waiting for a 409. `null` when payments are accepted.
    #[schema(example = "payments.blocker.card_payments_inactive")]
    pub cannot_accept_reason: Option<String>,
    /// The connected account's settlement currency. The storefront must
    /// price in this currency; anything else is refused at creation.
    #[schema(example = "JPY")]
    pub currency: Option<String>,
    /// Whether a checkout started now would have automatic tax applied,
    /// read from the tenant's stored tax settings by the same rule the
    /// session creation applies. The storefront renders its tax note from
    /// this; it never assumes tax is on.
    pub tax_enabled: bool,
}

/// One payment as the tenant sees it in the list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PaymentView {
    #[schema(value_type = String)]
    pub id: PaymentId,
    pub external_payment_id: String,
    #[schema(value_type = Option<String>)]
    pub checkout_id: Option<CheckoutId>,
    pub status: String,
    pub amount_minor: i64,
    pub currency: String,
    /// The provider-reported fee on this payment. When the platform's
    /// application fee is enabled for the tenant, this is where it is
    /// disclosed — a platform fee the merchant cannot see is not
    /// acceptable, whatever the contract says.
    pub fee_minor: Option<i64>,
    pub fee_currency: Option<String>,
    /// Payout status once payout reconciliation includes this payment
    /// (`pending` | `paid` | `failed`; null until then).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payout_status: Option<String>,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    #[schema(value_type = Option<String>)]
    pub captured_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub created_at: Option<DateTime<Utc>>,
}

/// One refund record.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RefundView {
    #[schema(value_type = String)]
    pub id: RefundId,
    #[schema(value_type = String)]
    pub payment_id: PaymentId,
    pub external_refund_id: Option<String>,
    pub amount_minor: i64,
    pub currency: String,
    pub status: String,
    pub reason: Option<String>,
    #[schema(value_type = Option<String>)]
    pub created_by: Option<UserId>,
    #[schema(value_type = String)]
    pub created_at: DateTime<Utc>,
}

/// One dispute plus its dashboard deep link (read-only).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DisputeView {
    #[schema(value_type = String)]
    pub id: DisputeId,
    #[schema(value_type = String)]
    pub payment_id: PaymentId,
    pub external_dispute_id: String,
    pub status: String,
    pub amount_minor: i64,
    pub currency: String,
    pub reason: Option<String>,
    #[schema(value_type = Option<String>)]
    pub due_by: Option<DateTime<Utc>>,
    pub charge_id: Option<String>,
    pub dashboard_url: Option<String>,
    #[schema(value_type = String)]
    pub created_at: DateTime<Utc>,
}

/// The payment detail the support UI renders: payment + its refunds +
/// disputes + timeline.
///
/// The timeline is a first-class projection derived from stored events
/// (created, succeeded/failed, refunded, disputed, paid out), each with
/// a timestamp and an actor where one exists.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PaymentDetailView {
    pub payment: PaymentView,
    pub refunds: Vec<RefundView>,
    pub disputes: Vec<DisputeView>,
    pub timeline: Vec<TimelineEntry>,
    pub checkout_reference: Option<String>,
}

/// Cursor-paginated payment list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PaymentListView {
    pub items: Vec<PaymentView>,
    pub next_cursor: Option<String>,
}

/// Cursor-paginated dispute list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DisputeListView {
    pub items: Vec<DisputeView>,
    pub next_cursor: Option<String>,
}

/// Query for `GET /api/v1/tenant/payments`.
#[derive(Debug, Clone, Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PaymentListQuery {
    /// Filter by payment status.
    pub status: Option<String>,
    /// Filter by payout status.
    pub payout_status: Option<String>,
    /// Filter by currency (ISO-4217).
    pub currency: Option<String>,
    /// Filter by customer reference substring.
    pub customer: Option<String>,
    /// Filter by created_at >= (RFC3339).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub date_from: Option<DateTime<Utc>>,
    /// Filter by created_at <= (RFC3339).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub date_to: Option<DateTime<Utc>>,
    /// Pagination cursor (opaque).
    #[serde(rename = "after", alias = "cursor")]
    pub cursor: Option<String>,
    /// Page size, clamped to 1..=100.
    #[param(schema_with = payment_limit_schema)]
    pub limit: Option<u32>,
}

/// Query for `GET /api/v1/tenant/payments/disputes`.
#[derive(Debug, Clone, Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DisputeListQuery {
    /// Filter by dispute status.
    pub status: Option<String>,
    /// Filter by currency (ISO-4217).
    pub currency: Option<String>,
    /// Filter by created_at >= (RFC3339).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub date_from: Option<DateTime<Utc>>,
    /// Filter by created_at <= (RFC3339).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub date_to: Option<DateTime<Utc>>,
    /// Pagination cursor (opaque).
    #[serde(rename = "after", alias = "cursor")]
    pub cursor: Option<String>,
    /// Page size, clamped to 1..=100.
    #[param(schema_with = payment_limit_schema)]
    pub limit: Option<u32>,
}

/// Body for `POST /api/v1/tenant/payments/{id}/refund`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RefundRequest {
    /// Minor-unit amount or `null` for a full refund (remaining amount).
    pub amount: Option<RefundAmount>,
    /// Why the refund is being issued — required, audited and shown in
    /// the timeline. Empty is rejected.
    pub reason: String,
    /// Optional free-form note (support context, not the audit reason).
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RefundAmount {
    #[schema(example = 500)]
    pub minor_units: i64,
    #[schema(example = "JPY", value_type = String)]
    pub currency: String,
}

/// One payout as the tenant's list renders it. Read-only: payout
/// management lives in the provider's own dashboard / embedded component.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PayoutView {
    pub external_payout_id: String,
    pub amount_minor: i64,
    pub currency: String,
    /// `pending` | `paid` | `failed`. A failed payout is alerted, not
    /// just listed — the tenant's money is stuck at the provider.
    #[schema(example = "paid")]
    pub status: String,
    #[schema(value_type = Option<String>)]
    pub arrival_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PayoutsListView {
    /// Newest first; empty before a connection exists.
    pub payouts: Vec<PayoutView>,
}

/// The tenant-facing automatic-tax state plus the standing legal
/// statement: registration and remittance are the tenant's responsibility;
/// Sanvi never advises on where or whether to register.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaxSettingsView {
    pub enabled: bool,
    /// The liable connected-account id when enabled (`{ type: 'account' }`
    /// routing) — with direct charges the connected account is the
    /// merchant of record.
    pub liability_account: Option<String>,
    pub provider_status: String,
    pub active_registrations: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<TaxWarningView>,
    #[schema(value_type = Option<String>)]
    pub last_checked_at: Option<DateTime<Utc>>,
    /// Always present, always the same: we never advise.
    pub disclaimer: &'static str,
    /// The platform's fee on this tenant's payments, when one is enabled
    /// (off by default in 0.10.0). Disclosed here and on every affected
    /// payment — a platform fee the merchant cannot see is not acceptable,
    /// whatever the contract says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_fee: Option<PlatformFeeDisclosure>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaxWarningView {
    /// Stable i18n key (`payments.tax.warning.*`).
    pub code: String,
    pub detail: String,
    #[schema(value_type = String)]
    pub detected_at: DateTime<Utc>,
}

/// The platform's fee disclosure for this tenant (their payment settings).
/// A platform fee the merchant cannot see is not acceptable, whatever the
/// contract says.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct PlatformFeeDisclosure {
    /// Fees are off for this tenant unless stated here.
    pub enabled: bool,
    pub basis_points: Option<u32>,
    pub fixed_minor: Option<i64>,
    pub fixed_currency: Option<String>,
    pub minimum_minor: i64,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateTaxSettingsRequest {
    /// Enable requires the compliance preflight to pass right now; disable
    /// always succeeds (and is a tenant-facing change requiring notice:
    /// mid-period it changes what customers are charged).
    pub enabled: bool,
}

/// A minor-unit amount: `2000` + `JPY` is ¥2,000; `1999` + `USD` is
/// $19.99. Amounts are non-negative (direction is the caller's concept).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = PaymentsMoney)]
pub struct Money {
    /// Integer count of the currency's minor units — ¥2,000 is `2000`.
    #[schema(example = 2000)]
    pub amount_minor: i64,
    /// Uppercase ISO-4217 code; zero-decimal currencies are honored.
    #[schema(example = "JPY", value_type = String)]
    pub currency: String,
}

/// One entry in the payment's timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TimelineEntry {
    /// Stable kind: `created`, `succeeded`, `failed`, `canceled`,
    /// `refunded`, `disputed`, `dispute_closed`, `paid_out` (09.6).
    #[schema(example = "refunded")]
    pub kind: String,
    #[schema(value_type = String)]
    pub at: DateTime<Utc>,
    /// Who caused this entry, when a human exists (refunds). Webhook-
    /// derived entries have no actor — the provider did.
    #[schema(value_type = Option<String>)]
    pub actor: Option<UserId>,
    /// Money attached to the entry (payment total, refund portion,
    /// dispute amount), when relevant.
    pub amount: Option<Money>,
    /// Human detail (failure code, refund reason, dispute reason,
    /// status detail), when relevant.
    pub detail: Option<String>,
}

fn payment_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(100, 20).build()
}

#[cfg(test)]
mod pagination_contract_tests {
    use super::*;
    #[test]
    fn canonical_after_and_legacy_cursor_deserialize_identically() {
        for name in ["after", "cursor"] {
            let value = serde_json::json!({name: "opaque-next-page"});
            let payment: PaymentListQuery = serde_json::from_value(value.clone()).unwrap();
            let dispute: DisputeListQuery = serde_json::from_value(value).unwrap();
            assert_eq!(payment.cursor.as_deref(), Some("opaque-next-page"));
            assert_eq!(dispute.cursor.as_deref(), Some("opaque-next-page"));
        }
    }
}
