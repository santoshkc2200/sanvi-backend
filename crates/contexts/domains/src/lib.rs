//! The `domains` context: custom domains, DNS verification and TLS.
//!
//! Phase 08. Two journeys end in the same state — a verified,
//! TLS-terminated hostname that resolves to the tenant's storefront:
//!
//! * **Connect** — the tenant owns the domain already; we emit exact DNS
//!   records, verify ownership (TXT) and routing (CNAME/A) by polling
//!   multiple DNS-over-HTTPS resolvers, then authorize the edge to issue a
//!   certificate on demand.
//! * **Purchase** — the tenant buys through Sanvi; a registrar port
//!   searches, quotes and registers; a DNS provider port creates the zone
//!   and applies records; the sandbox reference adapters make the whole
//!   saga deterministic and testable without external accounts.
//!
//! The TLS termination itself lives in the edge proxy (Caddy/Traefik in the
//! reference deployment) behind the `/internal/tls/authorize` gate: the app
//! never holds private keys, it only vouches for hostnames it serves.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::{DomainsConfig, RegistrarConfig, TldConfig};

pub use domain::{
    Certificate, CertificateStatus, CustomDomain, DnsRecord, DnsRecordType, DomainErrorCode,
    DomainFailure, DomainKind, DomainOrder, DomainRole, DomainStatus, DomainsError, DomainsEvent,
    DomainsEventName, Hostname, Money, OrderStatus, RegistrantContact, TldPrice,
    VerificationChallenge,
    registrar::{DomainQuote, DomainRegistrarPort, RegistrationResult, RenewalResult},
};

/// Wire domains routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::routes::{
        domains_internal_routes, domains_platform_routes, domains_tenant_scope_routes,
    };
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::routes::PERMISSIONED_ROUTES;
}
