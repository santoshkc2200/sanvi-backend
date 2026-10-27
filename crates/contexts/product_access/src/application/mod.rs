//! Use case: forward a platform-authenticated request to a product service
//! with a freshly signed delegation.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::{RoleId, TenantId, UserId};

use crate::domain::{
    DelegationClaims, ForwardRequest, ForwardResponse, ProductAccessError, ProductUpstream,
    RolePermissions, TenantEntitlements, TokenSigner,
};

/// The verified caller as the platform resolved it for this request.
#[derive(Debug, Clone, Copy)]
pub struct Caller<'a> {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    /// Roles of the user's *active* membership in the tenant; empty for
    /// non-members.
    pub role_ids: &'a [RoleId],
}

pub struct ProxyRequest {
    entitlements: Arc<dyn TenantEntitlements>,
    permissions: Arc<dyn RolePermissions>,
    signer: Arc<dyn TokenSigner>,
    upstream: Arc<dyn ProductUpstream>,
    clock: fn() -> DateTime<Utc>,
}

impl ProxyRequest {
    pub fn new(
        entitlements: Arc<dyn TenantEntitlements>,
        permissions: Arc<dyn RolePermissions>,
        signer: Arc<dyn TokenSigner>,
        upstream: Arc<dyn ProductUpstream>,
    ) -> Self {
        Self {
            entitlements,
            permissions,
            signer,
            upstream,
            clock: Utc::now,
        }
    }

    /// Checks the tenant's entitlement and the user's product permissions
    /// (failing closed when either lookup fails), signs the delegation and
    /// forwards. Nothing reaches the product service when a check fails.
    #[tracing::instrument(
        name = "product_access.proxy",
        skip_all,
        fields(product = self.upstream.product().key, tenant_id = %caller.tenant_id)
    )]
    pub async fn execute(
        &self,
        caller: Caller<'_>,
        request: &ForwardRequest,
    ) -> Result<ForwardResponse, ProductAccessError> {
        let product = self.upstream.product();
        let entitlements = self.entitlements.resolve(caller.tenant_id).await?;
        let permissions = if caller.role_ids.is_empty() {
            Vec::new()
        } else {
            self.permissions
                .permissions(caller.tenant_id, caller.role_ids)
                .await?
        };
        let jti = uuid::Uuid::now_v7().simple().to_string();
        let claims = DelegationClaims::grant(
            product,
            caller.tenant_id,
            caller.user_id,
            &permissions,
            &entitlements,
            jti,
            (self.clock)(),
        )?;
        let token = self.signer.sign(&claims)?;
        self.upstream.forward(request, &token).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::domain::{ECOMMERCE, EntitlementView, Product, ProductPath};

    struct Fixed(Vec<EntitlementView>, Vec<String>);

    #[async_trait]
    impl TenantEntitlements for Fixed {
        async fn resolve(&self, _: TenantId) -> Result<Vec<EntitlementView>, ProductAccessError> {
            Ok(self.0.clone())
        }
    }

    #[async_trait]
    impl RolePermissions for Fixed {
        async fn permissions(
            &self,
            _: TenantId,
            _: &[RoleId],
        ) -> Result<Vec<String>, ProductAccessError> {
            Ok(self.1.clone())
        }
    }

    struct JsonSigner;

    impl TokenSigner for JsonSigner {
        fn sign(&self, claims: &DelegationClaims) -> Result<String, ProductAccessError> {
            Ok(serde_json::to_string(claims).unwrap())
        }

        fn sign_privacy(
            &self,
            claims: &crate::domain::PrivacyClaims,
        ) -> Result<String, ProductAccessError> {
            Ok(serde_json::to_string(claims).unwrap())
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait]
    impl ProductUpstream for Recorder {
        fn product(&self) -> &'static Product {
            &ECOMMERCE
        }

        async fn forward(
            &self,
            _: &ForwardRequest,
            identity_token: &str,
        ) -> Result<ForwardResponse, ProductAccessError> {
            self.0.lock().unwrap().push(identity_token.to_owned());
            Ok(ForwardResponse {
                status: 200,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            })
        }
    }

    fn proxy(enabled: bool, permissions: &[&str]) -> (Arc<Recorder>, ProxyRequest) {
        let fixed = Arc::new(Fixed(
            vec![
                EntitlementView {
                    key: "products.ecommerce.enabled".into(),
                    enabled,
                    limit: None,
                },
                EntitlementView {
                    key: "ecommerce.stores.max".into(),
                    enabled: true,
                    limit: Some(2),
                },
            ],
            permissions.iter().map(|p| (*p).to_owned()).collect(),
        ));
        let upstream = Arc::new(Recorder::default());
        let proxy = ProxyRequest::new(fixed.clone(), fixed, Arc::new(JsonSigner), upstream.clone());
        (upstream, proxy)
    }

    fn request() -> ForwardRequest {
        ForwardRequest::new(
            "GET",
            ProductPath::parse("stores").unwrap(),
            None,
            [],
            Vec::new(),
            None,
        )
        .unwrap()
    }

    fn caller(role_ids: &[RoleId]) -> Caller<'_> {
        Caller {
            tenant_id: TenantId::try_from(1001_i64).unwrap(),
            user_id: UserId::try_from(3001_i64).unwrap(),
            role_ids,
        }
    }

    fn role() -> RoleId {
        RoleId::try_from(7_i64).unwrap()
    }

    #[tokio::test]
    async fn every_delegation_carries_a_fresh_jti() {
        let (upstream, proxy) = proxy(true, &["ecommerce.store.read"]);
        proxy.execute(caller(&[role()]), &request()).await.unwrap();
        proxy.execute(caller(&[role()]), &request()).await.unwrap();
        let tokens = upstream.0.lock().unwrap().clone();
        let jti = |t: &str| {
            serde_json::from_str::<serde_json::Value>(t).unwrap()["jti"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(jti(&tokens[0]).len(), 32);
        assert_ne!(jti(&tokens[0]), jti(&tokens[1]));
    }

    #[tokio::test]
    async fn forwards_with_a_delegation_for_this_tenant_and_user() {
        let (upstream, proxy) = proxy(true, &["ecommerce.store.read", "identity.member.read"]);
        let roles = [RoleId::try_from(7_i64).unwrap()];
        let response = proxy.execute(caller(&roles), &request()).await.unwrap();
        assert_eq!(response.status, 200);
        let tokens = upstream.0.lock().unwrap();
        let claims: serde_json::Value = serde_json::from_str(&tokens[0]).unwrap();
        assert_eq!(claims["tenant_id"], "1001");
        assert_eq!(claims["sub"], "3001");
        assert_eq!(
            claims["permissions"],
            serde_json::json!(["ecommerce.store.read"])
        );
        assert_eq!(claims["limits"]["ecommerce.stores.max"], 2);
    }

    #[tokio::test]
    async fn nothing_is_forwarded_without_the_product_or_a_membership() {
        let roles = [RoleId::try_from(7_i64).unwrap()];
        let (upstream, unentitled) = proxy(false, &["ecommerce.store.read"]);
        assert!(matches!(
            unentitled.execute(caller(&roles), &request()).await,
            Err(ProductAccessError::NotEntitled { .. })
        ));
        let (upstream_2, entitled) = proxy(true, &["ecommerce.store.read"]);
        assert!(matches!(
            entitled.execute(caller(&[]), &request()).await,
            Err(ProductAccessError::NoPermissions { .. })
        ));
        assert!(upstream.0.lock().unwrap().is_empty());
        assert!(upstream_2.0.lock().unwrap().is_empty());
    }
}
