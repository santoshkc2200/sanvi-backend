//! The payments context's HTTP state: one `Arc` per use case, assembled
//! in the composition root.

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::adapters::PaymentsMetrics;
use crate::application::{
    CreateAccountSession, CreateCheckout, DisconnectConnection, ExportPayments, GetPaymentDetail,
    IssueRefund, ListDisputes, ListPayments, ListPayouts, ListProviders, ManageTaxSettings,
    ProviderRegistry, StartProviderConnection, WebhookReceiver,
};
use crate::config::PaymentsConfig;
use crate::domain::{
    CheckoutRepository, CheckoutThrottle, DisputeRepository, PaymentRepository, PayoutRepository,
    ProviderConnectionRepository, RefundRepository, TaxSettingsRepository,
};

#[derive(Clone)]
pub struct PaymentsState {
    pub config: PaymentsConfig,
    pub metrics: Arc<PaymentsMetrics>,
    pub list_providers: Arc<ListProviders>,
    /// The provider registry — shared source of adapters for every use
    /// case. The catalog is *derived* from it; nothing else may enumerate
    /// providers.
    pub registry: Arc<ProviderRegistry>,
    /// Tenant-scoped reads/writes for the connection aggregate.
    pub tx: TxManager,
    pub connections: Arc<dyn ProviderConnectionRepository>,
    /// `POST /payments/connections` (09.2).
    pub create_connection: Arc<StartProviderConnection>,
    /// `POST /payments/connections/{id}/session` (09.2).
    pub account_session: Arc<CreateAccountSession>,
    /// `DELETE /payments/connections/{id}` (09.7): guards, local
    /// disconnect, event + audit.
    pub disconnect_connection: Arc<DisconnectConnection>,
    /// `POST /internal/webhooks/stripe/connect` (09.3): verify → store →
    /// ack; the worker processes what lands here.
    pub webhook_receiver: Arc<WebhookReceiver>,
    /// Tenant-scoped persistence for the checkout aggregate (09.4).
    pub checkouts: Arc<dyn CheckoutRepository>,
    /// `POST /tenant/checkout` (09.4): validate → persist → create the
    /// provider session on the connected account.
    pub create_checkout: Arc<CreateCheckout>,
    /// The checkout throttle (09.4): per tenant / IP / customer-ref
    /// buckets, checked before any database or provider work.
    pub checkout_throttle: Arc<dyn CheckoutThrottle>,
    /// 09.5: payments, refunds & disputes read models.
    pub payments: Arc<dyn PaymentRepository>,
    pub refunds: Arc<dyn RefundRepository>,
    pub disputes: Arc<dyn DisputeRepository>,
    pub list_payments: Arc<ListPayments>,
    pub get_payment: Arc<GetPaymentDetail>,
    pub issue_refund: Arc<IssueRefund>,
    pub list_disputes: Arc<ListDisputes>,
    pub export_payments: Arc<ExportPayments>,
    /// 09.6: payout visibility (read-only) and tax settings.
    pub payouts: Arc<dyn PayoutRepository>,
    pub list_payouts: Arc<ListPayouts>,
    pub tax_settings: Arc<dyn TaxSettingsRepository>,
    pub manage_tax_settings: Arc<ManageTaxSettings>,
    /// The read-only view of the platform's fee configuration — the
    /// disclosure source for the tenant's payment settings.
    pub fee_store: Arc<dyn crate::domain::ApplicationFeeStore>,
}
