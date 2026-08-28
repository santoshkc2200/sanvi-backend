use crate::domain::{CustomDomainId, DomainOrderId};

#[derive(Debug, thiserror::Error)]
pub enum DomainsError {
    #[error("invalid hostname: {0}")]
    InvalidHostname(String),

    #[error("hostname is reserved: {0}")]
    HostnameReserved(String),

    #[error("hostname must not be a platform domain or subdomain of one: {0}")]
    HostnameOnPlatformDomain(String),

    #[error("hostname is already claimed by another tenant: {0}")]
    HostnameAlreadyClaimed(String),

    #[error("hostname is already registered to this tenant: {0}")]
    HostnameAlreadyRegistered(String),

    #[error("unknown domain: {0}")]
    UnknownDomain(CustomDomainId),

    #[error("unknown order: {0}")]
    UnknownOrder(DomainOrderId),

    #[error("the domain is not owned by this tenant: {0}")]
    DomainNotOwned(CustomDomainId),

    #[error("invalid state transition: {from} → {to}")]
    IllegalTransition { from: String, to: String },

    #[error("the domain must be live or degraded before it can do this: {0}")]
    DomainNotLive(String),

    #[error("this tenant already has a primary domain")]
    PrimaryAlreadyExists,

    #[error("a removed domain cannot be modified")]
    DomainRemoved,

    #[error("verification is rate-limited; the next automatic check is at {next:?}")]
    VerificationThrottled { next: chrono::DateTime<chrono::Utc> },

    #[error("verification is unavailable: {0}")]
    VerificationUnavailable(String),

    #[error("feature not enabled for this tenant: {feature}")]
    NotEntitled { feature: String },

    #[error("registrar failure: {0}")]
    Registrar(String),

    #[error("registrar does not serve this TLD: {0}")]
    UnsupportedTld(String),

    #[error("domain is not available: {0}")]
    DomainUnavailable(String),

    #[error("dns provider failure: {0}")]
    DnsProvider(String),

    #[error("certificate authority failure: {0}")]
    CertificateAuthority(String),

    #[error("no certificate exists for this domain yet")]
    NoCertificate,

    #[error("charge failure: {0}")]
    Charge(String),

    #[error("refund failure: {0}")]
    Refund(String),

    #[error("the order is in a state that does not allow this: {0}")]
    OrderNotModifiable(String),

    #[error("registrations are non-refundable once the registrar accepts them")]
    OrderNonRefundable,

    #[error("tls authorization denied: {0}")]
    TlsDenied(String),

    #[error("tls authorization is not configured on this deployment")]
    TlsNotConfigured,

    #[error("probe failure: {0}")]
    Probe(String),

    #[error("storage failure: {0}")]
    Storage(String),

    #[error("cache failure: {0}")]
    Cache(String),

    #[error("outbox failure: {0}")]
    Outbox(String),

    #[error("notification failure: {0}")]
    Notification(String),

    #[error("id generation failed: {0}")]
    IdGeneration(#[from] sanvi_kernel::IdGenerationError),
}

impl DomainsError {
    /// Infrastructure-style errors: the adapter boundary logs the detail and
    /// returns a sanitized 500 rather than leaking internals.
    pub fn is_internal(&self) -> bool {
        matches!(
            self,
            Self::Storage(_)
                | Self::Cache(_)
                | Self::Outbox(_)
                | Self::Probe(_)
                | Self::Notification(_)
                | Self::IdGeneration(_)
        )
    }
}

/// Wrap a persistence-layer error into the storage variant (the anyhow
/// results `TxManager`/`Tx` methods return).
pub fn storage(err: impl std::fmt::Display) -> DomainsError {
    DomainsError::Storage(err.to_string())
}
