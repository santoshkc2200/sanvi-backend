//! The payments context's HTTP state: one `Arc` per use case, assembled
//! in the composition root.

use std::sync::Arc;

use crate::adapters::PaymentsMetrics;
use crate::application::ListProviders;
use crate::config::PaymentsConfig;

#[derive(Clone)]
pub struct PaymentsState {
    pub config: PaymentsConfig,
    pub metrics: Arc<PaymentsMetrics>,
    pub list_providers: Arc<ListProviders>,
}
