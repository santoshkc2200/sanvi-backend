//! Product access: delegating platform-authenticated requests to separately
//! deployed product services (ecommerce first).

pub mod delegation;
pub mod error;
pub mod forwarding;
pub mod ports;
pub mod product;
pub mod shopper;

pub use delegation::{
    ConnectorClaims, DelegationClaims, DestinationStoreId, EntitlementView, PRIVACY_AUDIENCE,
    PrivacyClaims, TOKEN_LIFETIME_SECS, UNLIMITED,
};
pub use error::ProductAccessError;
pub use forwarding::{ForwardRequest, ForwardResponse, ProductPath};
pub use ports::{ProductUpstream, RolePermissions, TenantEntitlements, TokenSigner};
pub use product::{CONNECTORS, ECOMMERCE, ISSUER, Product};
pub use shopper::{
    CHECKOUT_AUDIENCE, CREDENTIAL_AUDIENCE, CheckoutClaims, CheckoutLine,
    DEFAULT_CREDENTIAL_TTL_SECS, HostTenants, PurchaseCredentialClaims, QuotedTerms, ShopperError,
    ShopperTenant, StoreBindings, UpstreamProblem, WEB_CHECKOUT_PERMISSIONS, WEB_CHECKOUT_USER_ID,
};
