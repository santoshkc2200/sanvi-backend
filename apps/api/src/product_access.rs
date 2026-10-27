//! Composition for the product-access context: its entitlement and
//! permission ports read the access context's resolver and role catalog.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_access::adapters::SqlxRoleCatalog;
use sanvi_access::application::ListTenantEntitlements;
use sanvi_identity::domain::RoleCatalog;
use sanvi_kernel::{RoleId, TenantId};
use sanvi_product_access::ProductAccessConfig;
use sanvi_product_access::adapters::http::ProductAccessState;
use sanvi_product_access::adapters::signer::DisabledSigner;
use sanvi_product_access::adapters::{Ed25519Signer, HttpProductUpstream};
use sanvi_product_access::application::{ProxyImportRequest, ProxyRequest};
use sanvi_product_access::domain::{
    CONNECTORS, ECOMMERCE, EntitlementView, ProductAccessError, RolePermissions,
    TenantEntitlements, TokenSigner,
};

use crate::AccessWiring;

struct AccessEntitlements(Arc<ListTenantEntitlements>);

#[async_trait]
impl TenantEntitlements for AccessEntitlements {
    async fn resolve(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<EntitlementView>, ProductAccessError> {
        let resolved = self
            .0
            .execute(tenant_id)
            .await
            .map_err(|err| ProductAccessError::Storage(err.to_string()))?;
        Ok(resolved
            .into_iter()
            .map(|entry| EntitlementView {
                key: entry.feature.to_string(),
                enabled: entry.enabled,
                limit: entry.limit,
            })
            .collect())
    }
}

struct AccessRolePermissions(SqlxRoleCatalog);

#[async_trait]
impl RolePermissions for AccessRolePermissions {
    async fn permissions(
        &self,
        tenant_id: TenantId,
        role_ids: &[RoleId],
    ) -> Result<Vec<String>, ProductAccessError> {
        self.0
            .permissions_for_roles(tenant_id, role_ids)
            .await
            .map_err(|err| ProductAccessError::Storage(err.to_string()))
    }
}

/// Builds the proxy state. Infallible like the media wiring: a product
/// whose configuration does not load (boot validation reports it) fails
/// closed with 503 on every request instead of serving unauthenticated.
pub fn build_product_access_state(
    config: &ProductAccessConfig,
    access: &AccessWiring,
) -> ProductAccessState {
    let signer: Arc<dyn TokenSigner> =
        match Ed25519Signer::from_seed_hex(&config.signing_key_id, &config.signing_key_seed) {
            Ok(signer) => Arc::new(signer),
            Err(err) => {
                if !config.ecommerce.base_url.is_empty() || !config.connectors.base_url.is_empty() {
                    tracing::error!(error = %err, "product access signing key unusable");
                }
                Arc::new(DisabledSigner)
            }
        };
    let upstream = HttpProductUpstream::new(
        &ECOMMERCE,
        &config.ecommerce.base_url,
        &config.ecommerce.service_key,
    )
    .or_else(|err| {
        tracing::error!(error = %err, "ecommerce upstream unusable");
        HttpProductUpstream::new(&ECOMMERCE, "", "")
    })
    .expect("an unconfigured upstream always builds");

    let connectors_upstream = HttpProductUpstream::new(
        &CONNECTORS,
        &config.connectors.base_url,
        &config.connectors.service_key,
    )
    .or_else(|err| {
        tracing::error!(error = %err, "connectors upstream unusable");
        HttpProductUpstream::new(&CONNECTORS, "", "")
    })
    .expect("an unconfigured upstream always builds");

    let entitlements = Arc::new(AccessEntitlements(
        access.state.list_tenant_entitlements.clone(),
    ));
    let permissions = Arc::new(AccessRolePermissions(SqlxRoleCatalog::new(
        access.tx_manager.clone(),
        access.role_repository.clone(),
    )));

    ProductAccessState {
        ecommerce: Arc::new(ProxyRequest::new(
            entitlements.clone(),
            permissions.clone(),
            signer.clone(),
            Arc::new(upstream),
        )),
        imports: Arc::new(ProxyImportRequest::new(
            entitlements,
            permissions,
            signer,
            Arc::new(connectors_upstream),
        )),
    }
}
