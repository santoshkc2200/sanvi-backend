//! Domain: entities, value objects, events, errors, ports.
//! No infrastructure imports allowed here (enforced by scripts/check-layering.sh).

pub mod certificate;
pub mod custom_domain;
pub mod error;
pub mod events;
pub mod hostname;
pub mod money;
pub mod order;
pub mod ports;
pub mod records;
pub mod registrar;

pub use certificate::{Certificate, CertificateId, CertificateStatus, CertificateTag};
pub use custom_domain::{
    CustomDomain, CustomDomainId, CustomDomainTag, DomainErrorCode, DomainFailure, DomainKind,
    DomainRole, DomainStatus, VerificationChallenge, verification_backoff,
};
pub use error::{DomainsError, storage};
pub use events::{DomainsEvent, DomainsEventName};
pub use hostname::Hostname;
pub use money::{Money, TldPrice};
pub use order::{DomainOrder, DomainOrderId, DomainOrderTag, OrderStatus, RegistrantContact};
pub use ports::{
    CertificateAuthorityPort, CertificateRepository, Clock, CustomDomainRepository, DnsCheckResult,
    DnsProviderPort, DnsRecordRepository, DnsVerifierPort, DomainFeatureGate,
    DomainOrderRepository, EventPublisher, GuideTranslator, LiveHostRef, ProbeResult,
    PurchaseCharger, StorefrontProbePort, TenantState,
};
pub use records::{DnsRecord, DnsRecordId, DnsRecordTag, DnsRecordType};
pub use registrar::{
    DomainQuote, DomainRegistrarPort, RegistrarGuideId, RegistrationResult, RenewalResult,
};

// The entitlement keys gate the two journeys; defaulted in `DomainsConfig`
// but canonical here so every call site uses the same constants.
pub const CONNECT_ENTITLEMENT_KEY: &str = "domains.custom";
pub const PURCHASE_ENTITLEMENT_KEY: &str = "domains.purchase";

/// The edge gateway `CNAME` targets (and apex fallback `A` records point
/// at). Used by instructions rendering and health checks.
pub const ROUTING_CNAME_TARGET_KEY: &str = "domains.records.cname_target";
pub const ROUTING_APEX_KEY: &str = "domains.records.apex_a";
