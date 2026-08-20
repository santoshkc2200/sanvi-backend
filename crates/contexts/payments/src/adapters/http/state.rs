//! The payments context's HTTP state: one `Arc` per use case, assembled
//! in the composition root.

use std::sync::Arc;

use crate::adapters::PaymentsMetrics;
use crate::application::{ListProviders, ProviderRegistry, StartProviderConnection};
use crate::config::PaymentsConfig;

#[derive(Clone)]
pub struct PaymentsState {
    pub config: PaymentsConfig,
    pub metrics: Arc<PaymentsMetrics>,
    pub list_providers: Arc<ListProviders>,
    /// The provider registry — shared source of adapters for every use
    /// case. The catalog is *derived* from it; nothing else may enumerate
    /// providers.
    pub registry: Arc<ProviderRegistry>,
    /// No route yet (09.2's `POST /payments/connections`); wired now so
    /// the registry → port → repo → outbox vertical is exercised by the
    /// integration suite end to end.
    pub start_connection: Arc<StartProviderConnection>,
}
