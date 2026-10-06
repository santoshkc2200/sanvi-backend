//! The domains context's ports.
//!
//! Two shapes, both `Send + Sync` traits with `Arc<dyn _>` handles:
//!
//! * **Persistence ports** take the transaction scope as an argument —
//!   tenant reads/writes a `&mut ScopedTx` (RLS-scoped), platform-wide
//!   worker sweeps a `&mut PlatformScope`, hot-path reads a `&PgPool`.
//! * **Gateway ports** (registrar, DNS provider, certificate authority,
//!   DoH checker, storefront prober) are async and fail with
//!   `DomainsError`; cross-context hooks (entitlements, charging, tenant
//!   state, translation, notifications) are ports the composition root
//!   adapts so this crate never depends on another context.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use sanvi_kernel::TenantId;
use sanvi_persistence::{PgPool, PlatformScope, ScopedTx};

use crate::domain::{
    Certificate, CustomDomain, CustomDomainId, DnsRecord, DomainOrder, DomainOrderId, DomainsError,
    DomainsEvent, Hostname,
};

// ---------------------------------------------------------------------------
// Persistence ports.
// ---------------------------------------------------------------------------

/// The hot-path live-hostname mapping (the projection the resolution hook
/// and the TLS authorize gate read without a scoped transaction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveHostRef {
    pub tenant_id: TenantId,
    pub hostname: String,
    pub status: String,
}

/// The custom-domain repository. Tenant-scoped methods take a `ScopedTx`;
/// platform/worker methods take a `PlatformScope` or a pool for hot reads.
#[async_trait]
pub trait CustomDomainRepository: Send + Sync {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        domain: &CustomDomain,
    ) -> Result<(), DomainsError>;

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: CustomDomainId,
    ) -> Result<Option<CustomDomain>, DomainsError>;

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<CustomDomain>, DomainsError>;

    async fn find_primary(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<CustomDomain>, DomainsError>;

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        domain: &CustomDomain,
    ) -> Result<(), DomainsError>;

    /// Platform-wide: a tenant's own active claim for a hostname (any state
    /// except removed), across the tenant's rows only.
    async fn find_active_by_hostname_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        normalized_hostname: &str,
    ) -> Result<Option<CustomDomain>, DomainsError>;

    /// Platform-wide: the holder of the global live claim for a hostname
    /// (verified/live states), if any tenant holds it.
    async fn find_live_claim_by_hostname(
        &self,
        scope: &mut PlatformScope<'_>,
        normalized_hostname: &str,
    ) -> Result<Option<CustomDomain>, DomainsError>;

    /// Hot path: the hostname → tenant mapping for resolution and the TLS
    /// authorize gate. Reads the live-hostname projection (unscoped pool
    /// connections cannot see RLS rows, so this must not read
    /// `custom_domains` directly).
    async fn find_live_by_hostname(
        &self,
        pool: &PgPool,
        normalized_hostname: &str,
    ) -> Result<Option<LiveHostRef>, DomainsError>;

    /// Maintain the live-hostname projection on entry into the live set.
    async fn upsert_live_host(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        hostname: &str,
        status: &str,
    ) -> Result<(), DomainsError>;

    /// Maintain the live-hostname projection on exit from the live set.
    async fn delete_live_host(
        &self,
        scope: &mut PlatformScope<'_>,
        normalized_hostname: &str,
    ) -> Result<(), DomainsError>;

    /// Tenant-scope variant of the projection delete (removal paths run in
    /// the tenant scope; the projection itself has no RLS policy).
    async fn delete_live_host_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        normalized_hostname: &str,
    ) -> Result<(), DomainsError>;

    /// Worker: domains due for a verification poll.
    async fn due_for_verification(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CustomDomain>, DomainsError>;

    /// Worker: verified domains awaiting certificate issuance.
    async fn awaiting_cert(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<CustomDomain>, DomainsError>;

    /// Worker: live/degraded domains for the health sweep.
    async fn live_or_degraded(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<CustomDomain>, DomainsError>;

    /// Platform admin: search across tenants.
    async fn platform_search(
        &self,
        scope: &mut PlatformScope<'_>,
        query: &str,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<CustomDomain>, DomainsError>;

    /// Platform admin: read one domain across tenants.
    async fn platform_find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: CustomDomainId,
    ) -> Result<Option<CustomDomain>, DomainsError>;

    /// Update from a platform scope (platform release).
    async fn update_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain: &CustomDomain,
    ) -> Result<(), DomainsError>;
}

/// The purchase order repository.
#[async_trait]
pub trait DomainOrderRepository: Send + Sync {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        order: &DomainOrder,
    ) -> Result<(), DomainsError>;

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: DomainOrderId,
    ) -> Result<Option<DomainOrder>, DomainsError>;

    /// Idempotency lookup for the purchase command.
    async fn find_by_key(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        idempotency_key: Uuid,
    ) -> Result<Option<DomainOrder>, DomainsError>;

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<DomainOrder>, DomainsError>;

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        order: &DomainOrder,
    ) -> Result<(), DomainsError>;

    /// Worker sweeps write orders through the audited platform scope.
    async fn update_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        order: &DomainOrder,
    ) -> Result<(), DomainsError>;

    /// Worker: orders stuck mid-saga (charged without registration, or
    /// registered/configuring without an active domain).
    async fn due_for_advancement(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<DomainOrder>, DomainsError>;

    /// Worker: charged orders whose refund is pending a retry.
    async fn due_for_refund_retry(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<DomainOrder>, DomainsError>;

    /// Worker: active orders with auto-renew due.
    async fn due_for_renewal(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<DomainOrder>, DomainsError>;
}

/// The certificate repository.
#[async_trait]
pub trait CertificateRepository: Send + Sync {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        cert: &Certificate,
    ) -> Result<(), DomainsError>;

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        cert: &Certificate,
    ) -> Result<(), DomainsError>;

    async fn insert_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        cert: &Certificate,
    ) -> Result<(), DomainsError>;

    async fn update_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        cert: &Certificate,
    ) -> Result<(), DomainsError>;

    async fn find_for_domain(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        domain_id: CustomDomainId,
    ) -> Result<Option<Certificate>, DomainsError>;

    /// Latest certificate (by `not_after`) for several domains of one tenant
    /// in one statement, keyed by domain — the list view decorates every row
    /// with its cert, and a per-domain query there is an N+1. Domains with no
    /// certificate map to an absent entry.
    async fn find_for_domains(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        domain_ids: &[CustomDomainId],
    ) -> Result<std::collections::HashMap<CustomDomainId, Certificate>, DomainsError>;

    /// Platform scope read (force-release).
    async fn find_for_domain_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: CustomDomainId,
    ) -> Result<Option<Certificate>, DomainsError>;

    /// Worker: active certs due (or soon due) for renewal.
    async fn due_for_renewal(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        renew_before_days: u32,
        limit: u32,
    ) -> Result<Vec<Certificate>, DomainsError>;

    /// Worker: active certs for the health sweep (validity check).
    async fn active(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<Certificate>, DomainsError>;
}

/// The DNS records store (the provider adapter's backing store; also the
/// source of truth for purchased-domain verification).
#[async_trait]
pub trait DnsRecordRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: CustomDomainId,
        records: &[DnsRecord],
    ) -> Result<(), DomainsError>;

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        domain_id: CustomDomainId,
    ) -> Result<Vec<DnsRecord>, DomainsError>;

    /// Platform-scope read (worker verification sweeps).
    async fn list_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: CustomDomainId,
    ) -> Result<Vec<DnsRecord>, DomainsError>;

    async fn remove_zone(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: CustomDomainId,
    ) -> Result<(), DomainsError>;

    /// Platform-scope zone deletion (force-release).
    async fn remove_zone_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: CustomDomainId,
    ) -> Result<(), DomainsError>;
}

// ---------------------------------------------------------------------------
// Gateway ports.
// ---------------------------------------------------------------------------

/// Authoritative DNS management for purchased domains. Connected domains
/// stay at the customer's registrar — we never ask for those credentials.
#[async_trait]
pub trait DnsProviderPort: Send + Sync {
    /// Create the authoritative zone for the hostname's registrable domain.
    async fn create_zone(
        &self,
        scope: &mut ScopedTx<'_>,
        hostname: &Hostname,
    ) -> Result<(), DomainsError>;

    /// Apply (upsert) records to the zone.
    async fn apply_records(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: CustomDomainId,
        records: &[DnsRecord],
    ) -> Result<(), DomainsError>;

    /// Read back the records we manage (verification source of truth).
    async fn records(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: CustomDomainId,
    ) -> Result<Vec<DnsRecord>, DomainsError>;

    /// Platform-scope read (worker verification sweeps).
    async fn records_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: CustomDomainId,
    ) -> Result<Vec<DnsRecord>, DomainsError>;

    /// Delete the zone (domain removed / force-released).
    async fn remove_zone(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: CustomDomainId,
    ) -> Result<(), DomainsError>;

    /// Platform-scope zone deletion (force-release).
    async fn remove_zone_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: CustomDomainId,
    ) -> Result<(), DomainsError>;
}

/// The result of one verification check against the resolver set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DnsCheckResult {
    /// At least `min_agreement` resolvers agreed on the expected state.
    pub confirmed: bool,
    pub resolvers_checked: usize,
    pub resolvers_agreed: usize,
    pub detail: String,
}

/// DNS verification: TXT ownership and routing (CNAME/A/NS) checks against
/// multiple independent resolvers. Implemented by the DoH adapter; mocked
/// in tests.
#[async_trait]
pub trait DnsVerifierPort: Send + Sync {
    /// The TXT challenge record at `_sanvi-challenge.<hostname>` equals
    /// `sanvi-verify=<token>` on enough resolvers.
    async fn verify_txt(
        &self,
        hostname: &Hostname,
        expected_value: &str,
    ) -> Result<DnsCheckResult, DomainsError>;

    /// Routing: the hostname CNAMEs to `cname_target` (or, for apex, its
    /// A records point at one of `anycast_ips`).
    async fn verify_routing(
        &self,
        hostname: &Hostname,
        cname_target: &str,
        anycast_ips: &[String],
    ) -> Result<DnsCheckResult, DomainsError>;

    /// The zone's NS records (registrar detection for localized guides).
    async fn nameservers(&self, hostname: &Hostname) -> Result<Option<Vec<String>>, DomainsError>;
}

/// The certificate authority: issue, renew, revoke. The sandbox adapter is
/// deterministic; a production adapter drives the edge's ACME pipeline.
#[async_trait]
pub trait CertificateAuthorityPort: Send + Sync {
    async fn issue(&self, domain: &CustomDomain) -> Result<Certificate, DomainsError>;

    async fn renew(
        &self,
        cert: &Certificate,
        domain: &CustomDomain,
    ) -> Result<Certificate, DomainsError>;

    async fn revoke(&self, cert: &Certificate) -> Result<(), DomainsError>;
}

/// The result of one storefront probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProbeResult {
    pub ok: bool,
    pub status: Option<u16>,
    pub redirects: u32,
    pub detail: String,
}

/// Storefront reachability (the health monitor's HTTP check). The adapter
/// must be SSRF-safe: no internal ranges, no redirects to private IPs.
#[async_trait]
pub trait StorefrontProbePort: Send + Sync {
    async fn probe(&self, hostname: &Hostname) -> Result<ProbeResult, DomainsError>;
}

// ---------------------------------------------------------------------------
// Cross-context ports (adapted in the composition root).
// ---------------------------------------------------------------------------

/// The access context's entitlement gate, sliced to what domains needs.
#[async_trait]
pub trait DomainFeatureGate: Send + Sync {
    async fn is_enabled(&self, tenant_id: TenantId, feature: &str) -> Result<bool, DomainsError>;
}

/// One-off charging through the billing context (purchases ride the
/// platform subscription). The adapter owns charge + refund together so no
/// caller can forget either half of a compensation.
#[async_trait]
pub trait PurchaseCharger: Send + Sync {
    async fn charge(
        &self,
        tenant_id: TenantId,
        order_id: DomainOrderId,
        amount_minor: i64,
        currency: &str,
        description: &str,
    ) -> Result<String, DomainsError>;

    /// Refund a charge (registration failed after the charge succeeded).
    async fn refund(
        &self,
        tenant_id: TenantId,
        order_id: DomainOrderId,
        charge_id: &str,
    ) -> Result<(), DomainsError>;
}

/// Localized registrar guides and instruction copy. Scoped to the platform
/// catalog (`"platform"`), keys live in `catalogs/{locale}/domains.json`.
pub trait GuideTranslator: Send + Sync {
    fn translate(&self, locale: &str, key: &str, args: &[(&str, String)]) -> String;
}

/// Tenant lifecycle state, needed by the TLS authorize gate ("verified for
/// an active tenant").
#[async_trait]
pub trait TenantState: Send + Sync {
    async fn is_active(&self, tenant_id: TenantId) -> Result<bool, DomainsError>;
}

/// Publishes domain events into the transactional outbox.
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        events: &[DomainsEvent],
    ) -> Result<(), DomainsError>;

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        events: &[DomainsEvent],
    ) -> Result<(), DomainsError>;
}

/// The clock (fake-able in tests).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
