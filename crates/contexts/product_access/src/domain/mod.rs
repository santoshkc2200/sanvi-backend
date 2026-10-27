//! Product access: delegating platform-authenticated requests to separately
//! deployed product services (ecommerce first).

pub mod delegation;
pub mod error;
pub mod forwarding;
pub mod ports;
pub mod product;

pub use delegation::{
    ConnectorClaims, DelegationClaims, DestinationStoreId, EntitlementView, PRIVACY_AUDIENCE,
    PrivacyClaims, TOKEN_LIFETIME_SECS, UNLIMITED,
};
pub use error::ProductAccessError;
pub use forwarding::{ForwardRequest, ForwardResponse, ProductPath};
pub use ports::{ProductUpstream, RolePermissions, TenantEntitlements, TokenSigner};
pub use product::{CONNECTORS, ECOMMERCE, ISSUER, Product};
