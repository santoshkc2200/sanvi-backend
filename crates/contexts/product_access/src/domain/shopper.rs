//! The shopper path: the public, unauthenticated surface the storefront
//! app calls. The monolith resolves the tenant from the validated host,
//! checks the tenant's entitlement, binds the host to at most one
//! published store, orchestrates guest checkout against ecommerce-service
//! as the platform's own service user, and mints the short-lived purchase
//! credential that is a guest's only proof of order ownership.
//!
//! Two signed-token shapes live here, both verified by the monolith
//! itself (never forwarded to a service):
//!
//! - **Checkout claims** — the server-priced checkout summary a
//!   checkout-start returns as an opaque token and the shopper presents
//!   back at placement. Carrying the snapshot inside the token keeps the
//!   orchestrator stateless: placement re-checks the catalog against the
//!   token's prices and refuses a drift (`price-changed`) instead of
//!   silently charging a different total.
//! - **Purchase credential** — minted at placement, scoped to one order,
//!   short-lived. An order id alone authorizes nothing.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::error::ProductAccessError;
use super::product::ISSUER;

/// Audience of checkout-summary tokens (verified by this monolith).
pub const CHECKOUT_AUDIENCE: &str = "sanvi-backend/ecommerce-checkout";
/// Audience of purchase credentials (verified by this monolith).
pub const CREDENTIAL_AUDIENCE: &str = "sanvi-backend/ecommerce-orders";

/// The synthetic subject the shopper path acts as toward ecommerce-service —
/// the architecture doc's "web checkout's service user". The service only
/// requires a parseable positive id; no member account exists behind it.
pub const WEB_CHECKOUT_USER_ID: i64 = 9_000_000_000_000_000_001;

/// The permission set the web-checkout service user is delegated. The
/// narrowest set the orchestration needs: catalog and stock reads, guest
/// customer/address management, reservation holds, order placement/reads/
/// cancellation and hosted payment attempts.
pub const WEB_CHECKOUT_PERMISSIONS: &[&str] = &[
    "ecommerce.store.read",
    "ecommerce.product.read",
    "ecommerce.inventory.read",
    "ecommerce.stock.reserve",
    "ecommerce.customer.read",
    "ecommerce.customer.manage",
    "ecommerce.order.place",
    "ecommerce.order.read",
    "ecommerce.order.manage",
    "ecommerce.payment.collect",
];

/// How long a purchase credential grants order access. Short-lived by
/// design — the status link states its expiry to the shopper — but long
/// enough to check on a delivery.
pub const DEFAULT_CREDENTIAL_TTL_SECS: i64 = 86_400;

/// Why a shopper request failed. Handlers map these onto the shared
/// problem+json envelope; `Upstream` re-emits the service's own typed
/// problem (slug and extensions) so the public taxonomy stays the
/// service's taxonomy for every refusal the service made.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ShopperError {
    /// The host resolved to a tenant that publishes no purchasable store:
    /// not entitled, no published store, an archived store, or a tenant
    /// that is not active. Deliberately one shape — never an upgrade
    /// prompt, never an enumeration signal.
    #[error("no store is available for this host")]
    StoreUnavailable,
    /// A resource in an otherwise valid store does not exist.
    #[error("no such {resource}")]
    NotFound { resource: &'static str },
    /// The purchase credential is absent, malformed, or scoped to another
    /// order or tenant. One class: it must not reveal which.
    #[error("order access requires a valid purchase credential")]
    CredentialInvalid,
    /// The purchase credential's expiry has passed.
    #[error("the order link has expired")]
    CredentialExpired,
    /// The checkout reference is malformed, belongs to another host, or
    /// the checkout input failed validation; `detail` names why.
    #[error("{detail}")]
    CheckoutInvalid { detail: String },
    /// The checkout's hold has expired; re-validation is the recovery.
    #[error("the checkout has expired")]
    CheckoutExpired,
    /// The placed order's prices differ from the checkout summary the
    /// shopper approved. The order is cancelled and the hold released.
    #[error("prices have changed since checkout started")]
    PriceChanged,
    /// The service refused with a typed problem; re-emitted verbatim.
    #[error("{0}")]
    Upstream(#[from] Box<UpstreamProblem>),
    /// The product service is not configured, unreachable or timed out.
    #[error("the store service is temporarily unavailable: {0}")]
    Unavailable(String),
    /// The service answered in a way that means misconfiguration.
    #[error("the store service answered unexpectedly: {0}")]
    BadGateway(String),
    /// Platform-side lookups (tenant, entitlements, binding storage) failed.
    #[error("storage failure: {0}")]
    Storage(String),
}

impl From<ProductAccessError> for ShopperError {
    fn from(error: ProductAccessError) -> Self {
        match error {
            ProductAccessError::NotEntitled { .. } => Self::StoreUnavailable,
            ProductAccessError::NoPermissions { .. } => Self::StoreUnavailable,
            ProductAccessError::BadRequest(detail) => Self::BadGateway(detail),
            ProductAccessError::Unavailable(detail) => Self::Unavailable(detail),
            ProductAccessError::BadGateway(detail) => Self::BadGateway(detail),
            ProductAccessError::Storage(detail) => Self::Storage(detail),
        }
    }
}

/// A problem document the service answered with, kept in the service's own
/// vocabulary. `type` stays the service's full URI; the handler re-emits
/// the document (status, slug, extensions) so the storefront's failure
/// taxonomy branches on the same slugs the service defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamProblem {
    pub status: u16,
    /// Full problem `type` URI as the service emitted it.
    pub type_uri: String,
    pub title: String,
    pub detail: String,
    /// Field-addressed issues (`fields: [{field, message}]`), if any.
    pub fields: Vec<(String, String)>,
    /// Line indexes of a stock refusal (`ecommerce/insufficient-stock`).
    pub lines: Vec<i64>,
    /// The pending attempt a `payment-in-progress` refusal names.
    pub attempt_id: Option<String>,
    /// Version the refusing order actually carries (version conflicts).
    pub current_version: Option<i64>,
    /// Amount still owed on a payment refusal.
    pub due_minor: Option<i64>,
    /// Amount a pending attempt holds on a payment refusal.
    pub pending_minor: Option<i64>,
    /// The order an `already-ordered` refusal names.
    pub order_id: Option<String>,
    /// The customer an `email-taken` refusal names.
    pub customer_id: Option<String>,
}

impl std::fmt::Display for UpstreamProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.title, self.type_uri)
    }
}

impl std::error::Error for UpstreamProblem {}

impl UpstreamProblem {
    /// The slug below the shared problem base (for example
    /// `ecommerce/insufficient-stock`).
    pub fn slug(&self) -> &str {
        const BASE: &str = "https://sanvi.dev/problems/";
        self.type_uri.strip_prefix(BASE).unwrap_or(&self.type_uri)
    }

    /// Parses a problem document from a service response body. `None` when
    /// the body is not a problem document (the caller then treats the
    /// response as an unexpected upstream answer).
    pub fn parse(status: u16, body: &[u8]) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let type_uri = value.get("type")?.as_str()?.to_owned();
        let title = value
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Error")
            .to_owned();
        let detail = value
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let fields = value
            .get("fields")
            .and_then(|v| v.as_array())
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|field| {
                        let name = field.get("field")?.as_str()?.to_owned();
                        let message = field.get("message")?.as_str()?.to_owned();
                        Some((name, message))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let lines = value
            .get("lines")
            .and_then(|v| v.as_array())
            .map(|lines| lines.iter().filter_map(|l| l.as_i64()).collect())
            .unwrap_or_default();
        let get_u64_string = |key: &str| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .filter(|s| !s.is_empty())
        };
        Some(Self {
            status,
            type_uri,
            title,
            detail,
            fields,
            lines,
            attempt_id: get_u64_string("attempt_id"),
            current_version: value.get("current_version").and_then(|v| v.as_i64()),
            due_minor: value.get("due_minor").and_then(|v| v.as_i64()),
            pending_minor: value.get("pending_minor").and_then(|v| v.as_i64()),
            order_id: get_u64_string("order_id"),
            customer_id: get_u64_string("customer_id"),
        })
    }
}

/// A problem body the orchestrator mints itself, in the shopper slug space.
impl ShopperError {
    /// The problem slug below the shared base, when this error carries a
    /// context-owned class (not an upstream pass-through).
    pub fn slug(&self) -> Option<&'static str> {
        match self {
            Self::StoreUnavailable => Some("ecommerce/store-unavailable"),
            Self::NotFound { .. } => Some("not-found"),
            Self::CredentialInvalid => Some("ecommerce/credential-invalid"),
            Self::CredentialExpired => Some("ecommerce/credential-expired"),
            Self::CheckoutInvalid { .. } => Some("bad-request"),
            Self::CheckoutExpired => Some("ecommerce/checkout-expired"),
            Self::PriceChanged => Some("ecommerce/price-changed"),
            Self::Upstream(_) | Self::Unavailable(_) | Self::BadGateway(_) | Self::Storage(_) => {
                None
            }
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::StoreUnavailable => 404,
            Self::NotFound { .. } => 404,
            Self::CredentialInvalid | Self::CredentialExpired => 401,
            Self::CheckoutInvalid { .. } => 400,
            Self::CheckoutExpired | Self::PriceChanged => 409,
            Self::Upstream(problem) => problem.status,
            Self::Unavailable(_) => 503,
            Self::BadGateway(_) => 502,
            Self::Storage(_) => 500,
        }
    }
}

/// The tenant a public host resolves to, as the shopper path may see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShopperTenant {
    pub tenant_id: TenantId,
    /// Whether the tenant is live (`Active`). Anything else serves no store.
    pub active: bool,
}

/// Host → tenant resolution for the public scope (implemented over the
/// tenancy context's resolver in the composition root).
#[async_trait]
pub trait HostTenants: Send + Sync {
    async fn resolve(&self, host: &str) -> Result<Option<ShopperTenant>, ProductAccessError>;
}

/// Which of a tenant's stores its hosts serve. At most one store per
/// tenant: the published store. Absence is the unbound-host case.
#[async_trait]
pub trait StoreBindings: Send + Sync {
    async fn published_store(&self, tenant_id: TenantId)
    -> Result<Option<i64>, ProductAccessError>;

    /// Publishes (`store_id`) or withdraws (`None`) the tenant's store.
    async fn publish(
        &self,
        tenant_id: TenantId,
        store_id: Option<i64>,
    ) -> Result<(), ProductAccessError>;
}

/// One priced line inside a checkout summary token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutLine {
    pub variant_id: String,
    pub unit_price_minor: i64,
    pub quantity: i64,
}

/// Signed checkout-summary claims. `exp` mirrors the reservation's
/// `expires_at`, so the token dies with the hold it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutClaims {
    pub iss: String,
    pub aud: String,
    /// The reservation id the checkout holds (decimal Snowflake).
    pub sub: String,
    pub tenant_id: String,
    pub store_id: String,
    pub customer_id: String,
    pub shipping_address_id: String,
    pub currency: String,
    pub lines: Vec<CheckoutLine>,
    pub jti: String,
    pub iat: i64,
    pub exp: i64,
}

impl CheckoutClaims {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reservation_id: String,
        tenant_id: TenantId,
        store_id: i64,
        customer_id: String,
        shipping_address_id: String,
        currency: String,
        lines: Vec<CheckoutLine>,
        jti: String,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            iss: ISSUER.to_owned(),
            aud: CHECKOUT_AUDIENCE.to_owned(),
            sub: reservation_id,
            tenant_id: tenant_id.to_string(),
            store_id: store_id.to_string(),
            customer_id,
            shipping_address_id,
            currency,
            lines,
            jti,
            iat: now.timestamp(),
            exp: expires_at.timestamp(),
        }
    }

    /// Interprets a signature-verified payload as checkout claims, refusing
    /// anything addressed to another audience.
    pub fn from_verified(value: serde_json::Value) -> Result<Self, ShopperError> {
        let claims: Self =
            serde_json::from_value(value).map_err(|_| ShopperError::CheckoutInvalid {
                detail: "the checkout reference is not valid".to_owned(),
            })?;
        if claims.aud != CHECKOUT_AUDIENCE || claims.iss != ISSUER {
            return Err(ShopperError::CheckoutInvalid {
                detail: "the checkout reference is not valid".to_owned(),
            });
        }
        Ok(claims)
    }
}

/// Signed purchase-credential claims: access to exactly one order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurchaseCredentialClaims {
    pub iss: String,
    pub aud: String,
    /// The order id the credential grants access to (decimal Snowflake).
    pub sub: String,
    pub tenant_id: String,
    pub store_id: String,
    pub jti: String,
    pub iat: i64,
    pub exp: i64,
}

impl PurchaseCredentialClaims {
    pub fn new(
        order_id: String,
        tenant_id: TenantId,
        store_id: i64,
        jti: String,
        ttl_secs: i64,
        now: DateTime<Utc>,
    ) -> Self {
        let iat = now.timestamp();
        Self {
            iss: ISSUER.to_owned(),
            aud: CREDENTIAL_AUDIENCE.to_owned(),
            sub: order_id,
            tenant_id: tenant_id.to_string(),
            store_id: store_id.to_string(),
            jti,
            iat,
            exp: iat + ttl_secs,
        }
    }

    /// Interprets a signature-verified payload as credential claims.
    pub fn from_verified(value: serde_json::Value) -> Result<Self, ShopperError> {
        let claims: Self =
            serde_json::from_value(value).map_err(|_| ShopperError::CredentialInvalid)?;
        if claims.aud != CREDENTIAL_AUDIENCE || claims.iss != ISSUER {
            return Err(ShopperError::CredentialInvalid);
        }
        Ok(claims)
    }

    /// Whether the credential is still alive at `now`.
    pub fn alive_at(&self, now: DateTime<Utc>) -> bool {
        now.timestamp() < self.exp
    }

    /// Checks a credential against the order it is presented for and the
    /// tenant the host resolved to. Absent/malformed/mismatched → invalid;
    /// otherwise expiry decides. This is the whole access decision for
    /// order-scoped reads — an order id alone authorizes nothing.
    pub fn authorize(
        value: Option<serde_json::Value>,
        order_id: &str,
        tenant_id: TenantId,
        now: DateTime<Utc>,
    ) -> Result<Self, ShopperError> {
        let value = value.ok_or(ShopperError::CredentialInvalid)?;
        let claims = Self::from_verified(value)?;
        if claims.sub != order_id || claims.tenant_id != tenant_id.to_string() {
            return Err(ShopperError::CredentialInvalid);
        }
        if !claims.alive_at(now) {
            return Err(ShopperError::CredentialExpired);
        }
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: DateTime<Utc> = DateTime::from_timestamp(1_800_000_000, 0).unwrap();

    fn tenant() -> TenantId {
        TenantId::try_from(1001_i64).unwrap()
    }

    #[test]
    fn checkout_claims_round_trip_and_audience_check() {
        let claims = CheckoutClaims::new(
            "555".to_owned(),
            tenant(),
            42,
            "700".to_owned(),
            "800".to_owned(),
            "JPY".to_owned(),
            vec![CheckoutLine {
                variant_id: "900".to_owned(),
                unit_price_minor: 1200,
                quantity: 2,
            }],
            "018f0000000070008000000000000000".to_owned(),
            DateTime::from_timestamp(1_800_000_900, 0).unwrap(),
            NOW,
        );
        assert_eq!(claims.exp - claims.iat, 900);
        let json = serde_json::to_value(&claims).unwrap();
        let parsed = CheckoutClaims::from_verified(json).unwrap();
        assert_eq!(parsed, claims);

        let mut wrong = serde_json::to_value(&claims).unwrap();
        wrong["aud"] = serde_json::json!("somewhere-else");
        assert_eq!(
            CheckoutClaims::from_verified(wrong),
            Err(ShopperError::CheckoutInvalid {
                detail: "the checkout reference is not valid".to_owned(),
            })
        );
    }

    #[test]
    fn credential_authorize_distinguishes_invalid_expired_and_wrong_order() {
        let claims = PurchaseCredentialClaims::new(
            "42".to_owned(),
            tenant(),
            7,
            "018f0000000070008000000000000000".to_owned(),
            3600,
            NOW,
        );
        let json = serde_json::to_value(&claims).unwrap();

        // The matching order and tenant, while alive.
        assert_eq!(
            PurchaseCredentialClaims::authorize(Some(json.clone()), "42", tenant(), NOW),
            Ok(claims.clone())
        );
        // A different order id: invalid, never "not found" (no enumeration).
        assert_eq!(
            PurchaseCredentialClaims::authorize(Some(json.clone()), "43", tenant(), NOW),
            Err(ShopperError::CredentialInvalid)
        );
        // A different tenant: invalid.
        assert_eq!(
            PurchaseCredentialClaims::authorize(
                Some(json.clone()),
                "42",
                TenantId::try_from(2002_i64).unwrap(),
                NOW
            ),
            Err(ShopperError::CredentialInvalid)
        );
        // Expired: its own class, honestly named.
        let later = DateTime::from_timestamp(1_800_000_000 + 3601, 0).unwrap();
        assert_eq!(
            PurchaseCredentialClaims::authorize(Some(json.clone()), "42", tenant(), later),
            Err(ShopperError::CredentialExpired)
        );
        // Absent and malformed: invalid.
        assert_eq!(
            PurchaseCredentialClaims::authorize(None, "42", tenant(), NOW),
            Err(ShopperError::CredentialInvalid)
        );
        assert_eq!(
            PurchaseCredentialClaims::authorize(Some(serde_json::json!(7)), "42", tenant(), NOW),
            Err(ShopperError::CredentialInvalid)
        );
    }

    #[test]
    fn upstream_problems_keep_the_service_slugs_and_extensions() {
        let body = serde_json::json!({
            "type": "https://sanvi.dev/problems/ecommerce/insufficient-stock",
            "title": "Insufficient stock",
            "status": 409,
            "detail": "some lines exceed available stock",
            "lines": [0, 2]
        })
        .to_string();
        let problem = UpstreamProblem::parse(409, body.as_bytes()).unwrap();
        assert_eq!(problem.slug(), "ecommerce/insufficient-stock");
        assert_eq!(problem.lines, vec![0, 2]);
        assert_eq!(problem.status, 409);
        assert!(UpstreamProblem::parse(500, b"not json").is_none());
        assert_eq!(
            problem.status, 409,
            "status stays the service's, not a reclassification"
        );
        assert!(ShopperError::Upstream(Box::new(problem)).slug().is_none());
    }

    #[test]
    fn shopper_error_statuses_match_the_public_contract() {
        assert_eq!(ShopperError::StoreUnavailable.status(), 404);
        assert_eq!(
            ShopperError::StoreUnavailable.slug(),
            Some("ecommerce/store-unavailable")
        );
        assert_eq!(ShopperError::CredentialInvalid.status(), 401);
        assert_eq!(
            ShopperError::CredentialExpired.slug(),
            Some("ecommerce/credential-expired")
        );
        assert_eq!(ShopperError::CheckoutExpired.status(), 409);
        assert_eq!(ShopperError::PriceChanged.status(), 409);
    }

    #[test]
    fn entitlement_and_permission_failures_render_store_unavailable() {
        // A shopper must never see an upgrade prompt: the entitlement-off
        // case renders the same "store unavailable" state as everything
        // else the host cannot serve.
        assert_eq!(
            ShopperError::from(ProductAccessError::NotEntitled {
                feature: "products.ecommerce.enabled"
            }),
            ShopperError::StoreUnavailable
        );
    }
}
