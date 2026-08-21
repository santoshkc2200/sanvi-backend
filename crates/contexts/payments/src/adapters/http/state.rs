//! The payments context's HTTP state: one `Arc` per use case, assembled
//! in the composition root.

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::adapters::PaymentsMetrics;
use crate::application::{
    CreateAccountSession, ListProviders, ProviderRegistry, StartProviderConnection,
};
use crate::config::PaymentsConfig;
use crate::domain::ProviderConnectionRepository;

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
}
