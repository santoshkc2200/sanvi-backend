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

/// Audience for privacy-scoped requests.
pub const PRIVACY_AUDIENCE: &str = "ecommerce-service/privacy";

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

/// A canonical, positive decimal store identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DestinationStoreId(i64);

impl DestinationStoreId {
    /// Parses a store identifier, accepting only canonical positive decimals
    /// (`1..=i64::MAX`, no leading zero, no sign).
    pub fn parse(s: &str) -> Result<Self, ProductAccessError> {
        let n = s.parse::<i64>().map_err(|_| {
            ProductAccessError::BadRequest(format!("invalid destination store id: {s:?}"))
        })?;
        if n > 0 && n.to_string() == s {
            Ok(Self(n))
        } else {
            Err(ProductAccessError::BadRequest(format!(
                "invalid destination store id: {s:?}"
            )))
        }
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

/// Checks that the tenant has the product enabled and the user holds at
/// least one permission for it. Returns the matching permissions sorted
/// and deduplicated.
fn authorize(
    product: &Product,
    permissions: &[String],
    entitlements: &[EntitlementView],
) -> Result<Vec<String>, ProductAccessError> {
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

    Ok(delegated)
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
    pub jti: String,
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
        jti: String,
        now: DateTime<Utc>,
    ) -> Result<Self, ProductAccessError> {
        let delegated = authorize(product, permissions, entitlements)?;

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
            jti,
            iat,
            exp: iat + TOKEN_LIFETIME_SECS,
        })
    }
}

/// Signed identity claims for the ecommerce-connectors service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectorClaims {
    pub tenant_id: i64,
    pub user_id: i64,
    pub destination_store_id: String,
    pub aud: &'static str,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

impl ConnectorClaims {
    /// Builds the claims for `user` in `tenant` against `store`, or refuses when
    /// the tenant lacks the product or the user holds none of its permissions.
    pub fn grant(
        product: &Product,
        tenant_id: TenantId,
        user_id: UserId,
        store: DestinationStoreId,
        permissions: &[String],
        entitlements: &[EntitlementView],
        jti: String,
        now: DateTime<Utc>,
    ) -> Result<Self, ProductAccessError> {
        let _ = authorize(product, permissions, entitlements)?;
        let iat = now.timestamp();
        Ok(Self {
            tenant_id: tenant_id.as_i64(),
            user_id: user_id.as_i64(),
            destination_store_id: store.as_i64().to_string(),
            aud: product.audience,
            iat,
            exp: iat + TOKEN_LIFETIME_SECS,
            jti,
        })
    }
}

/// Claims on a privacy delegation token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrivacyClaims {
    pub iss: &'static str,
    pub aud: String,
    pub sub: String,
    pub jti: String,
    pub iat: i64,
    pub exp: i64,
}

impl PrivacyClaims {
    /// Builds claims addressing the product's privacy audience for `user_id`.
    pub fn for_subject(
        product: &Product,
        user_id: UserId,
        jti: String,
        now: DateTime<Utc>,
    ) -> Self {
        let iat = now.timestamp();
        Self {
            iss: ISSUER,
            aud: format!("{}/privacy", product.audience),
            sub: user_id.to_string(),
            jti,
            iat,
            exp: iat + TOKEN_LIFETIME_SECS,
        }
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
            "test-jti-0000000000000000000000".to_owned(),
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
        assert_eq!(claims.jti, "test-jti-0000000000000000000000");
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

    #[test]
    fn privacy_claims_address_the_privacy_audience() {
        let claims = PrivacyClaims::for_subject(
            &ECOMMERCE,
            UserId::try_from(3001_i64).unwrap(),
            "018f0000000070008000000000000000".to_owned(),
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        );
        assert_eq!(claims.aud, "ecommerce-service/privacy");
        assert_eq!(claims.exp - claims.iat, 60);
    }

    #[test]
    fn connector_claims_match_the_connector_contract() {
        let claims = ConnectorClaims::grant(
            &crate::domain::product::CONNECTORS,
            TenantId::try_from(1001_i64).unwrap(),
            UserId::try_from(3001_i64).unwrap(),
            DestinationStoreId::parse("42").unwrap(),
            &[
                "ecommerce.import.read".to_owned(),
                "ecommerce.store.read".to_owned(),
            ],
            &[entitlement("products.ecommerce.enabled", true, None)],
            "test-jti-0000000000000000000000".to_owned(),
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        )
        .unwrap();

        let json = serde_json::to_value(&claims).unwrap();
        let obj = json.as_object().unwrap();
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "aud",
                "destination_store_id",
                "exp",
                "iat",
                "jti",
                "tenant_id",
                "user_id"
            ]
        );
        assert_eq!(obj["tenant_id"], 1001);
        assert_eq!(obj["user_id"], 3001);
        assert_eq!(obj["destination_store_id"], "42");
        assert_eq!(obj["aud"], "ecommerce-connectors");
        assert_eq!(claims.exp - claims.iat, 60);
    }

    #[test]
    fn connector_claims_refuse_without_the_product_or_an_import_permission() {
        let grant_connector = |perms: &[&str], ents: &[EntitlementView]| {
            let perms: Vec<String> = perms.iter().map(|p| (*p).to_owned()).collect();
            ConnectorClaims::grant(
                &crate::domain::product::CONNECTORS,
                TenantId::try_from(1001_i64).unwrap(),
                UserId::try_from(3001_i64).unwrap(),
                DestinationStoreId::parse("42").unwrap(),
                &perms,
                ents,
                "test-jti".to_owned(),
                DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            )
        };

        assert_eq!(
            grant_connector(&["ecommerce.import.read"], &[]),
            Err(ProductAccessError::NotEntitled {
                feature: "products.ecommerce.enabled"
            })
        );
        assert_eq!(
            grant_connector(
                &["ecommerce.import.read"],
                &[entitlement("products.ecommerce.enabled", false, None)]
            ),
            Err(ProductAccessError::NotEntitled {
                feature: "products.ecommerce.enabled"
            })
        );
        assert_eq!(
            grant_connector(
                &["ecommerce.store.read"],
                &[entitlement("products.ecommerce.enabled", true, None)]
            ),
            Err(ProductAccessError::NoPermissions {
                product: "ecommerce-imports"
            })
        );
    }

    #[test]
    fn store_ids_are_canonical_positive_decimals() {
        for valid in ["1", "42", "9223372036854775807"] {
            let store = DestinationStoreId::parse(valid).unwrap();
            assert_eq!(store.as_i64().to_string(), valid);
        }
        for invalid in [
            "",
            "0",
            "007",
            "-1",
            "+1",
            "1.0",
            "9223372036854775808",
            "%31",
            "a",
        ] {
            assert!(
                matches!(
                    DestinationStoreId::parse(invalid),
                    Err(ProductAccessError::BadRequest(_))
                ),
                "expected BadRequest for {invalid:?}"
            );
        }
    }
}
