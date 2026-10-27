pub mod audit;
mod credentials;
pub mod freee;
mod postgres;
pub mod privacy_provider;
pub use credentials::PostgresCredentialStore;
pub use freee::{FreeeClient, FreeeSettings};
pub use postgres::PostgresAccountingStore;

/// Fail-closed verifier for compositions without provider credentials.
pub struct UnavailableVerifier;

#[async_trait::async_trait]
impl crate::application::ProviderVerifier for UnavailableVerifier {
    async fn verify(
        &self,
        _: sanvi_kernel::TenantId,
        _: &crate::domain::Connection,
    ) -> Result<(), crate::domain::AccountingError> {
        Err(crate::domain::AccountingError::VerificationUnavailable)
    }
}
