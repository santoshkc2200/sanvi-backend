//! Domain ports: the interfaces adapters implement. Stripe specifics live
//! only in adapters (sub-phase 09.2); the proof the abstraction holds is
//! the fake provider in tests (09.1).

use async_trait::async_trait;
use uuid::Uuid;

use sanvi_persistence::ScopedTx;

use super::connection::ProviderConnection;
use super::error::PaymentsError;

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
}
