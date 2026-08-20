//! Application layer: use cases and their view models.
//!
//! 09.0 ships one use case — the provider catalog — and its port
//! (`ProviderCatalog`) returns an empty list until 09.1 fills it. The
//! point of the slice is that the route, the entitlement gate, the
//! contract and the generated frontend client all exist and are tested
//! before any provider logic lands on top of them.

pub mod catalog;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use catalog::{EmptyProviderCatalog, ListProviders, ProviderCatalog};

/// One connectable provider as the settings page renders it. Kept minimal
/// in 09.0 (the envelope is the contract); 09.1 fills in the real entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProviderView {
    /// Stable provider identifier (`stripe_connect`).
    #[schema(example = "stripe_connect")]
    pub provider: String,
    /// Human-readable name for cards and menus.
    #[schema(example = "Stripe")]
    pub display_name: String,
}

/// `GET /api/v1/tenant/payments/providers` — the catalog envelope. The
/// empty array in 09.0 is the contract's shape, not a placeholder for a
/// different one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProvidersView {
    /// Connectable providers; empty until the catalog lands (09.1).
    pub providers: Vec<ProviderView>,
}
