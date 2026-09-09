//! Small stubs for composition roots that need only part of a context's
//! ports (the worker drives `configure_dns` without the purchase gate).

use async_trait::async_trait;

use sanvi_kernel::TenantId;

use crate::domain::{DomainFeatureGate, DomainOrderId, DomainsError, PurchaseCharger};

/// A gate that denies everything: for use in processes where the purchase
/// flow itself is never reachable (the worker only resumes orders; the API
/// owns the user-facing journey).
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysDisabledGate;

#[async_trait]
impl DomainFeatureGate for AlwaysDisabledGate {
    async fn is_enabled(&self, _tenant_id: TenantId, _feature: &str) -> Result<bool, DomainsError> {
        Ok(false)
    }
}

/// The reference purchase charger: deterministic sandbox charges recorded
/// in the logs (and, through the API's audit trail, in `platform.audit_log`
/// on the surrounding platform-scope transactions). A production adapter
/// implements the same port over the billing context's one-off charge
/// capability; the saga's compensation logic (refund paths, idempotency on
/// the order id) is exercised against this adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct SandboxPurchaseCharger;

#[async_trait]
impl PurchaseCharger for SandboxPurchaseCharger {
    async fn charge(
        &self,
        _tenant_id: TenantId,
        order_id: DomainOrderId,
        amount_minor: i64,
        currency: &str,
        description: &str,
    ) -> Result<String, DomainsError> {
        tracing::info!(
            order_id = %order_id,
            amount_minor,
            currency,
            description,
            "sandbox charge recorded"
        );
        Ok(format!("sandbox-ch-{order_id}"))
    }

    async fn refund(
        &self,
        _tenant_id: TenantId,
        order_id: DomainOrderId,
        charge_id: &str,
    ) -> Result<(), DomainsError> {
        tracing::info!(order_id = %order_id, charge_id, "sandbox refund recorded");
        Ok(())
    }
}

/// A disabled purchase charger that rejects all charge and refund operations.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledPurchaseCharger;

#[async_trait]
impl PurchaseCharger for DisabledPurchaseCharger {
    async fn charge(
        &self,
        _tenant_id: TenantId,
        _order_id: DomainOrderId,
        _amount_minor: i64,
        _currency: &str,
        _description: &str,
    ) -> Result<String, DomainsError> {
        Err(DomainsError::Charge(
            "purchase charger capability is disabled".to_string(),
        ))
    }

    async fn refund(
        &self,
        _tenant_id: TenantId,
        _order_id: DomainOrderId,
        _charge_id: &str,
    ) -> Result<(), DomainsError> {
        Err(DomainsError::Refund(
            "purchase charger capability is disabled".to_string(),
        ))
    }
}

/// A disabled certificate authority that rejects all issuance and renewal operations.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledCertificateAuthority;

#[async_trait]
impl crate::domain::CertificateAuthorityPort for DisabledCertificateAuthority {
    async fn issue(
        &self,
        _domain: &crate::domain::CustomDomain,
    ) -> Result<crate::domain::Certificate, DomainsError> {
        Err(DomainsError::CertificateAuthority(
            "certificate authority capability is disabled".to_string(),
        ))
    }

    async fn renew(
        &self,
        _cert: &crate::domain::Certificate,
        _domain: &crate::domain::CustomDomain,
    ) -> Result<crate::domain::Certificate, DomainsError> {
        Err(DomainsError::CertificateAuthority(
            "certificate authority capability is disabled".to_string(),
        ))
    }

    async fn revoke(&self, _cert: &crate::domain::Certificate) -> Result<(), DomainsError> {
        Err(DomainsError::CertificateAuthority(
            "certificate authority capability is disabled".to_string(),
        ))
    }
}

/// A disabled domain registrar that rejects all catalog, search, quote, register, and renew operations.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledRegistrar;

#[async_trait]
impl crate::domain::DomainRegistrarPort for DisabledRegistrar {
    fn registrar_id(&self) -> String {
        "disabled".to_string()
    }

    fn catalog(&self) -> Vec<crate::domain::TldPrice> {
        Vec::new()
    }

    async fn search(&self, _query: &str) -> Result<Vec<crate::domain::DomainQuote>, DomainsError> {
        Err(DomainsError::Registrar(
            "domain registrar capability is disabled".to_string(),
        ))
    }

    async fn quote(
        &self,
        _hostname: &crate::domain::Hostname,
        _term_years: u8,
    ) -> Result<crate::domain::DomainQuote, DomainsError> {
        Err(DomainsError::Registrar(
            "domain registrar capability is disabled".to_string(),
        ))
    }

    async fn register(
        &self,
        _order_id: DomainOrderId,
        _hostname: &crate::domain::Hostname,
        _term_years: u8,
        _whois_privacy: bool,
    ) -> Result<crate::domain::RegistrationResult, DomainsError> {
        Err(DomainsError::Registrar(
            "domain registrar capability is disabled".to_string(),
        ))
    }

    async fn renew(
        &self,
        _external_ref: &str,
        _hostname: &crate::domain::Hostname,
        _term_years: u8,
    ) -> Result<crate::domain::RenewalResult, DomainsError> {
        Err(DomainsError::Registrar(
            "domain registrar capability is disabled".to_string(),
        ))
    }
}
