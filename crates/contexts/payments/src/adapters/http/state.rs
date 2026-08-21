//! The payments context's HTTP state: one `Arc` per use case, assembled
//! in the composition root.

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::adapters::PaymentsMetrics;
use crate::application::{
    CreateAccountSession, CreateCheckout, ListProviders, ProviderRegistry, StartProviderConnection,
    WebhookReceiver,
};
use crate::config::PaymentsConfig;
use crate::domain::{CheckoutRepository, CheckoutThrottle, ProviderConnectionRepository};

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
}
