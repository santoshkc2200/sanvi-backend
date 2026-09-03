//! Application layer: use cases and their view models.
//!
//! 09.1 fills the catalog in: the provider registry is the single source
//! of catalog entries, `StartProviderConnection` proves the
//! registry → port → repo → outbox vertical, and the Stripe adapter
//! (09.2) slots in behind the same ports.

pub mod account_session;
pub mod catalog;
pub mod create_checkout;
pub mod create_connection;
pub mod disconnect;
pub mod disputes;
pub mod export;
pub mod fees;
pub mod lifecycle;
pub mod list_payments;
pub mod list_payouts;
pub mod refund;
pub mod registry;
pub mod sync_connection;
pub mod tax_settings;
pub mod webhooks;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use account_session::CreateAccountSession;
pub use catalog::{EmptyProviderCatalog, ListProviders, ProviderCatalog, RegistryProviderCatalog};
pub use create_checkout::{
    ApplicationFeePolicy, CheckoutOutcome, CheckoutReadiness, CreateCheckout, NewCheckoutCommand,
    NoApplicationFee, conversion_event_id,
};
pub use create_connection::{StartConnectionCommand, StartProviderConnection};
pub use disconnect::{DisconnectCommand, DisconnectConnection, TENANT_REQUEST_REASON};
pub use disputes::{DisputeListItem, DisputePageView, ListDisputes, ListDisputesCommand};
pub use export::{CSV_COLUMNS, ExportCommand, ExportPayments};
pub use fees::{ConfiguredApplicationFeePolicy, TenantFeeDisclosure, disclose_fee_for};
pub use lifecycle::{DEADLINE_REMINDER_WINDOW_DAYS, apply_account_snapshot, target_steps};
pub use list_payments::{GetPaymentDetail, ListPayments, ListPaymentsCommand, PaymentDetail};
pub use list_payouts::{ListPayouts, PayoutView as PayoutItemView, PayoutsView};
pub use refund::{IssueRefund, RefundCommand};
pub use registry::{ProviderRegistry, RuntimeEnvironment};
pub use sync_connection::{SyncConnections, SyncReport};
pub use tax_settings::{
    CheckTenantTaxCompliance, ComplianceReport, ManageTaxSettings, TaxSettingsView, TaxWarningView,
};
pub use webhooks::{DISPATCH_BATCH, MAX_WEBHOOK_ATTEMPTS, WebhookDispatcher, WebhookReceiver};

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
