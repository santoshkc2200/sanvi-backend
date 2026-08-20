//! Application layer: use cases and their view models.
//!
//! 09.1 fills the catalog in: the provider registry is the single source
//! of catalog entries, `StartProviderConnection` proves the
//! registry → port → repo → outbox vertical, and the Stripe adapter
//! (09.2) slots in behind the same ports.

pub mod catalog;
pub mod connections;
pub mod registry;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use catalog::{EmptyProviderCatalog, ListProviders, ProviderCatalog, RegistryProviderCatalog};
pub use connections::{ResumeProviderOnboarding, StartConnectionCommand, StartProviderConnection};
pub use registry::{ProviderRegistry, RuntimeEnvironment};

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
