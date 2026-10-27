use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::Caller;
use crate::domain::{
    ConnectorClaims, DestinationStoreId, ForwardRequest, ForwardResponse, ProductAccessError,
    ProductUpstream, RolePermissions, TenantEntitlements, TokenSigner,
};

pub struct ProxyImportRequest {
    entitlements: Arc<dyn TenantEntitlements>,
    permissions: Arc<dyn RolePermissions>,
    signer: Arc<dyn TokenSigner>,
    upstream: Arc<dyn ProductUpstream>,
    clock: fn() -> DateTime<Utc>,
}

impl ProxyImportRequest {
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

    #[tracing::instrument(
        name = "product_access.import_proxy",
        skip_all,
        fields(tenant_id = %caller.tenant_id, store_id = store.as_i64())
    )]
    pub async fn execute(
        &self,
        caller: Caller<'_>,
        store: DestinationStoreId,
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
        let claims = ConnectorClaims::grant(
            product,
            caller.tenant_id,
            caller.user_id,
            store,
            &permissions,
            &entitlements,
            jti,
            (self.clock)(),
        )?;
        let token = self.signer.sign_connector(&claims)?;
        self.upstream.forward(request, &token).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use sanvi_kernel::{RoleId, TenantId, UserId};

    use super::*;
    use crate::domain::{CONNECTORS, EntitlementView, Product, ProductPath};

    enum EntitlementBehavior {
        Fixed(Vec<EntitlementView>),
        Fail,
    }

    struct FakeEntitlements(EntitlementBehavior);

    #[async_trait]
    impl TenantEntitlements for FakeEntitlements {
        async fn resolve(&self, _: TenantId) -> Result<Vec<EntitlementView>, ProductAccessError> {
            match &self.0 {
                EntitlementBehavior::Fixed(views) => Ok(views.clone()),
                EntitlementBehavior::Fail => {
                    Err(ProductAccessError::Storage("db connection failed".into()))
                }
            }
        }
    }

    struct FakePermissions(Vec<String>);

    #[async_trait]
    impl RolePermissions for FakePermissions {
        async fn permissions(
            &self,
            _: TenantId,
            _: &[RoleId],
        ) -> Result<Vec<String>, ProductAccessError> {
            Ok(self.0.clone())
        }
    }

    struct JsonSigner;

    impl TokenSigner for JsonSigner {
        fn sign(
            &self,
            claims: &crate::domain::DelegationClaims,
        ) -> Result<String, ProductAccessError> {
            Ok(serde_json::to_string(claims).unwrap())
        }

        fn sign_privacy(
            &self,
            claims: &crate::domain::PrivacyClaims,
        ) -> Result<String, ProductAccessError> {
            Ok(serde_json::to_string(claims).unwrap())
        }

        fn sign_connector(&self, claims: &ConnectorClaims) -> Result<String, ProductAccessError> {
            Ok(serde_json::to_string(claims).unwrap())
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait]
    impl ProductUpstream for Recorder {
        fn product(&self) -> &'static Product {
            &CONNECTORS
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

    fn proxy(
        entitlement_behavior: EntitlementBehavior,
        permissions: &[&str],
    ) -> (Arc<Recorder>, ProxyImportRequest) {
        let entitlements = Arc::new(FakeEntitlements(entitlement_behavior));
        let permissions = Arc::new(FakePermissions(
            permissions.iter().map(|p| (*p).to_owned()).collect(),
        ));
        let upstream = Arc::new(Recorder::default());
        let proxy = ProxyImportRequest::new(
            entitlements,
            permissions,
            Arc::new(JsonSigner),
            upstream.clone(),
        );
        (upstream, proxy)
    }

    fn request() -> ForwardRequest {
        ForwardRequest::new(
            "GET",
            ProductPath::parse("jobs").unwrap(),
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

    fn store() -> DestinationStoreId {
        DestinationStoreId::parse("42").unwrap()
    }

    #[tokio::test]
    async fn forwards_with_a_connector_token_for_this_tenant_user_and_store() {
        let (upstream, proxy) = proxy(
            EntitlementBehavior::Fixed(vec![EntitlementView {
                key: "products.ecommerce.enabled".into(),
                enabled: true,
                limit: None,
            }]),
            &["ecommerce.import.read"],
        );
        let roles = [role()];
        let response = proxy
            .execute(caller(&roles), store(), &request())
            .await
            .unwrap();
        assert_eq!(response.status, 200);

        proxy
            .execute(caller(&roles), store(), &request())
            .await
            .unwrap();

        let tokens = upstream.0.lock().unwrap().clone();
        assert_eq!(tokens.len(), 2);

        let claims_1: serde_json::Value = serde_json::from_str(&tokens[0]).unwrap();
        let claims_2: serde_json::Value = serde_json::from_str(&tokens[1]).unwrap();

        assert_eq!(claims_1["tenant_id"], 1001);
        assert_eq!(claims_1["user_id"], 3001);
        assert_eq!(claims_1["destination_store_id"], "42");
        assert_eq!(claims_1["aud"], "ecommerce-connectors");

        let jti_1 = claims_1["jti"].as_str().unwrap();
        let jti_2 = claims_2["jti"].as_str().unwrap();
        assert_eq!(jti_1.len(), 32);
        assert_eq!(jti_2.len(), 32);
        assert_ne!(jti_1, jti_2);
    }

    #[tokio::test]
    async fn nothing_is_forwarded_without_the_product_or_an_import_permission() {
        let roles = [role()];
        let (upstream_unentitled, unentitled) = proxy(
            EntitlementBehavior::Fixed(vec![EntitlementView {
                key: "products.ecommerce.enabled".into(),
                enabled: false,
                limit: None,
            }]),
            &["ecommerce.import.read"],
        );
        assert!(matches!(
            unentitled
                .execute(caller(&roles), store(), &request())
                .await,
            Err(ProductAccessError::NotEntitled { .. })
        ));
        assert!(upstream_unentitled.0.lock().unwrap().is_empty());

        let (upstream_noperm, noperm) = proxy(
            EntitlementBehavior::Fixed(vec![EntitlementView {
                key: "products.ecommerce.enabled".into(),
                enabled: true,
                limit: None,
            }]),
            &["ecommerce.import.read"],
        );
        assert!(matches!(
            noperm.execute(caller(&[]), store(), &request()).await,
            Err(ProductAccessError::NoPermissions { .. })
        ));
        assert!(upstream_noperm.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_entitlement_lookup_fails_closed() {
        let roles = [role()];
        let (upstream, proxy) = proxy(EntitlementBehavior::Fail, &["ecommerce.import.read"]);
        assert!(matches!(
            proxy.execute(caller(&roles), store(), &request()).await,
            Err(ProductAccessError::Storage(_))
        ));
        assert!(upstream.0.lock().unwrap().is_empty());
    }
}
