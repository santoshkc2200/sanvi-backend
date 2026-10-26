//! The delegation a product service receives: who acts, in which tenant,
//! with which of their permissions and which plan limits.
//!
//! The wire shape is the identity-token contract in
//! `ecommerce-service/docs/architecture.md` ("identity token v2").

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use serde::Serialize;

use super::error::ProductAccessError;
use super::product::{ISSUER, Product};

/// How long a delegation token is valid. Services accept at most 120 s.
pub const TOKEN_LIFETIME_SECS: i64 = 60;

/// A quota entitlement that is enabled but has no limit is unlimited;
/// services compare counts against this ceiling.
pub const UNLIMITED: i64 = i64::MAX;

/// One resolved entitlement of a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitlementView {
    /// Feature key.
    pub key: String,
    /// Whether the tenant has it.
    pub enabled: bool,
    /// Quota limit; `None` on a quota means unlimited.
    pub limit: Option<i64>,
}

/// Signed-token claims. Ids travel as decimal strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DelegationClaims {
    pub iss: &'static str,
    pub aud: &'static str,
    pub sub: String,
    pub tenant_id: String,
    pub permissions: Vec<String>,
    pub limits: BTreeMap<String, i64>,
    pub iat: i64,
    pub exp: i64,
}

impl DelegationClaims {
    /// Builds the claims for `user` in `tenant`, or refuses when the tenant
    /// lacks the product or the user holds none of its permissions.
    pub fn grant(
        product: &Product,
        tenant_id: TenantId,
        user_id: UserId,
        permissions: &[String],
        entitlements: &[EntitlementView],
        now: DateTime<Utc>,
    ) -> Result<Self, ProductAccessError> {
        let enabled = entitlements
            .iter()
            .any(|entry| entry.key == product.feature && entry.enabled);
        if !enabled {
            return Err(ProductAccessError::NotEntitled {
                feature: product.feature,
            });
        }

        let mut delegated: Vec<String> = permissions
            .iter()
            .filter(|permission| permission.starts_with(product.permission_prefix))
            .cloned()
            .collect();
        delegated.sort();
        delegated.dedup();
        if delegated.is_empty() {
            return Err(ProductAccessError::NoPermissions {
                product: product.key,
            });
        }

        // A disabled or absent quota is left out, which services read as 0.
        let limits = product
            .limits
            .iter()
            .filter_map(|key| {
                entitlements
                    .iter()
                    .find(|entry| entry.key == *key && entry.enabled)
                    .map(|entry| ((*key).to_owned(), entry.limit.unwrap_or(UNLIMITED)))
            })
            .collect();

        let iat = now.timestamp();
        Ok(Self {
            iss: ISSUER,
            aud: product.audience,
            sub: user_id.to_string(),
            tenant_id: tenant_id.to_string(),
            permissions: delegated,
            limits,
            iat,
            exp: iat + TOKEN_LIFETIME_SECS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::product::ECOMMERCE;

    fn entitlement(key: &str, enabled: bool, limit: Option<i64>) -> EntitlementView {
        EntitlementView {
            key: key.to_owned(),
            enabled,
            limit,
        }
    }

    fn grant(
        permissions: &[&str],
        entitlements: &[EntitlementView],
    ) -> Result<DelegationClaims, ProductAccessError> {
        let permissions: Vec<String> = permissions.iter().map(|p| (*p).to_owned()).collect();
        DelegationClaims::grant(
            &ECOMMERCE,
            TenantId::try_from(1001_i64).unwrap(),
            UserId::try_from(3001_i64).unwrap(),
            &permissions,
            entitlements,
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        )
    }

    #[test]
    fn delegates_only_the_products_permissions_and_limits() {
        let claims = grant(
            &[
                "media.assets.manage",
                "ecommerce.store.read",
                "ecommerce.product.manage",
                "ecommerce.store.read",
            ],
            &[
                entitlement("products.ecommerce.enabled", true, None),
                entitlement("ecommerce.stores.max", true, Some(3)),
                entitlement("advertising.campaign_limit", true, Some(9)),
            ],
        )
        .unwrap();
        assert_eq!(
            claims.permissions,
            ["ecommerce.product.manage", "ecommerce.store.read"]
        );
        assert_eq!(
            claims.limits,
            BTreeMap::from([("ecommerce.stores.max".to_owned(), 3)])
        );
        assert_eq!(
            (
                claims.iss,
                claims.aud,
                claims.sub.as_str(),
                claims.tenant_id.as_str()
            ),
            ("sanvi-backend", "ecommerce-service", "3001", "1001")
        );
        assert_eq!(claims.exp - claims.iat, TOKEN_LIFETIME_SECS);
    }

    #[test]
    fn unlimited_quota_becomes_the_ceiling_and_disabled_quota_is_omitted() {
        let on = entitlement("products.ecommerce.enabled", true, None);
        let unlimited = grant(
            &["ecommerce.store.read"],
            &[on.clone(), entitlement("ecommerce.stores.max", true, None)],
        )
        .unwrap();
        assert_eq!(unlimited.limits["ecommerce.stores.max"], UNLIMITED);
        let disabled = grant(
            &["ecommerce.store.read"],
            &[on, entitlement("ecommerce.stores.max", false, Some(5))],
        )
        .unwrap();
        assert!(disabled.limits.is_empty());
    }

    #[test]
    fn refuses_without_the_product_or_any_of_its_permissions() {
        assert_eq!(
            grant(&["ecommerce.store.read"], &[]),
            Err(ProductAccessError::NotEntitled {
                feature: "products.ecommerce.enabled"
            })
        );
        assert_eq!(
            grant(
                &["ecommerce.store.read"],
                &[entitlement("products.ecommerce.enabled", false, None)]
            ),
            Err(ProductAccessError::NotEntitled {
                feature: "products.ecommerce.enabled"
            })
        );
        assert_eq!(
            grant(
                &["media.assets.manage"],
                &[entitlement("products.ecommerce.enabled", true, None)]
            ),
            Err(ProductAccessError::NoPermissions {
                product: "ecommerce"
            })
        );
    }
}
