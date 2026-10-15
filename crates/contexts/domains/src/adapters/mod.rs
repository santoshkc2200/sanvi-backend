//! Driven adapters: SQLx repositories, the sandbox registrar/DNS/CA
//! gateways, the DoH verifier, the SSRF-guarded storefront prober, the
//! resolution cache, outbox publishing and the privacy provider.

pub mod builders;
pub mod ca;
pub mod cache;
pub mod dns_checker;
pub mod dns_provider;
pub mod edge_ca;
pub mod events;
pub mod http;
pub mod metrics;
pub mod notification_intent;
pub mod privacy_provider;
pub mod prober;
pub mod registrar;
pub mod repos;
pub mod stubs;

pub use builders::{build_certificate_authority, build_domain_registrar, build_purchase_charger};
pub use ca::SandboxCertificateAuthority;
pub use cache::LiveDomainCache;
pub use dns_checker::DohDnsVerifier;
pub use dns_provider::SqlxDnsProvider;
pub use events::{OutboxEventPublisher, SystemClock, domains_event_handler};
pub use metrics::DomainsMetrics;
pub use notification_intent::notification_intent;
pub use prober::HttpProbeAdapter;
pub use registrar::SandboxRegistrar;
pub use repos::{
    SqlxCertificateRepository, SqlxCustomDomainRepository, SqlxDnsRecordRepository,
    SqlxDomainOrderRepository,
};
pub use stubs::{
    AlwaysDisabledGate, DisabledCertificateAuthority, DisabledPurchaseCharger, DisabledRegistrar,
    SandboxPurchaseCharger,
};
