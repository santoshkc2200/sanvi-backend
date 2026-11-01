//! Ports the proxy needs: other contexts' read models, a signer and the
//! product service itself.

use async_trait::async_trait;
use sanvi_kernel::{RoleId, TenantId};

use super::delegation::{ConnectorClaims, DelegationClaims, EntitlementView, PrivacyClaims};
use super::error::ProductAccessError;
use super::forwarding::{ForwardRequest, ForwardResponse};
use super::product::Product;
use super::shopper::{CheckoutClaims, PurchaseCredentialClaims};

/// The tenant's resolved entitlements (implemented over the access context).
#[async_trait]
pub trait TenantEntitlements: Send + Sync {
    async fn resolve(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<EntitlementView>, ProductAccessError>;
}

/// Permissions granted by roles in a tenant (implemented over the access
/// context's role catalog).
#[async_trait]
pub trait RolePermissions: Send + Sync {
    async fn permissions(
        &self,
        tenant_id: TenantId,
        role_ids: &[RoleId],
    ) -> Result<Vec<String>, ProductAccessError>;
}

/// Signs delegation claims as a compact JWT.
pub trait TokenSigner: Send + Sync {
    fn sign(&self, claims: &DelegationClaims) -> Result<String, ProductAccessError>;
    fn sign_privacy(&self, claims: &PrivacyClaims) -> Result<String, ProductAccessError>;
    fn sign_connector(&self, claims: &ConnectorClaims) -> Result<String, ProductAccessError>;
    /// Signs the shopper-facing checkout summary the storefront passes
    /// back opaquely at placement.
    fn sign_checkout(&self, claims: &CheckoutClaims) -> Result<String, ProductAccessError>;
    /// Signs a purchase credential: short-lived, scoped to one order.
    fn sign_credential(
        &self,
        claims: &PurchaseCredentialClaims,
    ) -> Result<String, ProductAccessError>;
    /// Verifies a token signed by this signer and returns its claims.
    /// Signature/format failures only — audience and expiry are the
    /// domain's decision, not the signer's.
    fn verify_json(&self, token: &str) -> Result<serde_json::Value, ProductAccessError>;
}

/// Sends a request to a product service with the platform's credentials.
#[async_trait]
pub trait ProductUpstream: Send + Sync {
    /// The product this upstream serves.
    fn product(&self) -> &'static Product;

    async fn forward(
        &self,
        request: &ForwardRequest,
        identity_token: &str,
    ) -> Result<ForwardResponse, ProductAccessError>;
}
