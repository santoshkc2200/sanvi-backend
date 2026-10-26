mod postgres;
pub mod privacy_provider;
pub use postgres::PostgresAccountingStore;

/// Production-safe default until an OAuth/credential adapter is composed.
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
