//! The domains context's integration tests.
//!
//! Real Postgres in containers, the `sanvi_app` role (NOBYPASSRLS), full
//! migrations — the same recipe the tenancy/theming/billing suites use.
//! External gateways (DoH checker, storefront prober, charger, tenant
//! state, clock) are scripted fakes; the registrar/DNS/CA sandbox
//! adapters and the SQLx repositories are real.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use sanvi_domains::adapters::{
    LiveDomainCache, OutboxEventPublisher, SandboxCertificateAuthority, SandboxRegistrar,
    SqlxCertificateRepository, SqlxCustomDomainRepository, SqlxDnsProvider,
    SqlxDnsRecordRepository, SqlxDomainOrderRepository,
};
use sanvi_domains::application::{
    AuthorizeTls, ClaimCustomDomain, PlaceDomainOrder, PromotePrimaryDomain, RemoveCustomDomain,
    RequestVerification, RunCertIssuanceSweep, RunHealthSweep, RunOrderSweep, RunRenewalSweep,
    RunVerificationSweep, SetOrderAutoRenew,
};
use sanvi_domains::config::DomainsConfig;
use sanvi_domains::domain::{
    CertificateAuthorityPort, CertificateRepository, Clock, CustomDomain, CustomDomainRepository,
    DnsCheckResult, DnsProviderPort, DnsRecordType, DnsVerifierPort, DomainFeatureGate,
    DomainOrderRepository, DomainQuote, DomainRegistrarPort, DomainRole, DomainStatus,
    DomainsError, EventPublisher, Hostname, Notifications, ProbeResult, PurchaseCharger,
    RegistrationResult, RenewalResult, StorefrontProbePort, TenantState, TldPrice,
};
use sanvi_persistence::{DatabaseConfig, Migrator, TxManager, build_pool};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

fn migrations_dir() -> std::path::PathBuf {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join("../../../migrations"))
        .unwrap_or_else(|_| std::path::PathBuf::from("migrations"))
}

async fn prepare() -> (
    sanvi_persistence::PgPool,
    testcontainers::ContainerAsync<Postgres>,
) {
    let node = Postgres::default().start().await.unwrap();
    let port = node.get_host_port_ipv4(5432).await.unwrap();

    let admin = sqlx::PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
    ))
    .await
    .unwrap();
    sqlx::query("CREATE ROLE sanvi_migrator LOGIN PASSWORD 'mig'")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE ROLE sanvi_app LOGIN PASSWORD 'app' NOBYPASSRLS")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE sanvi_test OWNER sanvi_migrator")
        .execute(&admin)
        .await
        .unwrap();

    let migrator_pool = sqlx::PgPool::connect(&format!(
        "postgres://sanvi_migrator:mig@127.0.0.1:{port}/sanvi_test"
    ))
    .await
    .unwrap();
    Migrator::new(migrations_dir())
        .run(&migrator_pool)
        .await
        .expect("migrations up");

    let app_pool = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_app".to_string(),
        password: "app".to_string(),
        migrations_dir: migrations_dir().to_string_lossy().to_string(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();

    (app_pool, node)
}

async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> Uuid {
    let tenant_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, $2, $3, 'active', 'jp', 'en')",
    )
    .bind(tenant_id)
    .bind(slug)
    .bind(format!("{slug} corp"))
    .execute(pool)
    .await
    .unwrap();
    tenant_id
}

// ---------------------------------------------------------------------------
// Scripted fakes.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct FakeVerifier {
    inner: Arc<std::sync::Mutex<FakeVerifierState>>,
}

#[derive(Default)]
struct FakeVerifierState {
    txt_ok: bool,
    routing_ok: bool,
    nameservers: Vec<String>,
}

impl FakeVerifier {
    fn new(txt_ok: bool, routing_ok: bool) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(FakeVerifierState {
                txt_ok,
                routing_ok,
                ..FakeVerifierState::default()
            })),
        }
    }

    fn nameservers(&self, names: &[&str]) {
        self.inner.lock().unwrap().nameservers =
            names.iter().map(|name| name.to_string()).collect();
    }
}

#[async_trait::async_trait]
impl DnsVerifierPort for FakeVerifier {
    async fn verify_txt(
        &self,
        _hostname: &Hostname,
        _expected_value: &str,
    ) -> Result<DnsCheckResult, DomainsError> {
        let state = self.inner.lock().unwrap();
        Ok(DnsCheckResult {
            confirmed: state.txt_ok,
            resolvers_checked: 2,
            resolvers_agreed: if state.txt_ok { 2 } else { 0 },
            detail: "fake".to_string(),
        })
    }

    async fn verify_routing(
        &self,
        _hostname: &Hostname,
        _cname_target: &str,
        _anycast_ips: &[String],
    ) -> Result<DnsCheckResult, DomainsError> {
        let state = self.inner.lock().unwrap();
        Ok(DnsCheckResult {
            confirmed: state.routing_ok,
            resolvers_checked: 2,
            resolvers_agreed: if state.routing_ok { 2 } else { 0 },
            detail: "fake".to_string(),
        })
    }

    async fn nameservers(&self, _hostname: &Hostname) -> Result<Option<Vec<String>>, DomainsError> {
        let names = self.inner.lock().unwrap().nameservers.clone();
        Ok((!names.is_empty()).then_some(names))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FakeTenantState {
    active: bool,
}

#[async_trait::async_trait]
impl TenantState for FakeTenantState {
    async fn is_active(&self, _tenant_id: Uuid) -> Result<bool, DomainsError> {
        Ok(self.active)
    }
}

#[derive(Debug, Clone, Default)]
struct FakeClock {
    now: Arc<std::sync::Mutex<DateTime<Utc>>>,
}

impl FakeClock {
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(std::sync::Mutex::new(now)),
        }
    }

    fn set(&self, now: DateTime<Utc>) {
        *self.now.lock().unwrap() = now;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }
}

#[derive(Debug, Clone, Default)]
struct FakeCharger {
    charges: Arc<std::sync::Mutex<Vec<(Uuid, i64)>>>,
    fail_charge: bool,
    fail_refund: bool,
}

#[async_trait::async_trait]
impl PurchaseCharger for FakeCharger {
    async fn charge(
        &self,
        _tenant_id: Uuid,
        order_id: Uuid,
        amount_minor: i64,
        _currency: &str,
        _description: &str,
    ) -> Result<String, DomainsError> {
        if self.fail_charge {
            return Err(DomainsError::Charge("simulated charge failure".to_string()));
        }
        self.charges.lock().unwrap().push((order_id, amount_minor));
        Ok(format!("ch-{order_id}"))
    }

    async fn refund(
        &self,
        _tenant_id: Uuid,
        _order_id: Uuid,
        _charge_id: &str,
    ) -> Result<(), DomainsError> {
        if self.fail_refund {
            return Err(DomainsError::Refund("simulated refund failure".to_string()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FakeGate {
    enabled: bool,
}

#[async_trait::async_trait]
impl DomainFeatureGate for FakeGate {
    async fn is_enabled(&self, _tenant_id: Uuid, _feature: &str) -> Result<bool, DomainsError> {
        Ok(self.enabled)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FakeProber {
    ok: bool,
}

#[async_trait::async_trait]
impl StorefrontProbePort for FakeProber {
    async fn probe(&self, _hostname: &Hostname) -> Result<ProbeResult, DomainsError> {
        Ok(ProbeResult {
            ok: self.ok,
            status: self.ok.then_some(200),
            redirects: 0,
            detail: String::new(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FakeNotifications;

#[async_trait::async_trait]
impl Notifications for FakeNotifications {
    async fn domain_degraded(&self, _h: &str, _c: &str, _d: &str) -> Result<(), DomainsError> {
        Ok(())
    }
    async fn certificate_renewal_failed(&self, _h: &str, _d: &str) -> Result<(), DomainsError> {
        Ok(())
    }
    async fn domain_expiring_soon(&self, _h: &str, _d: u32) -> Result<(), DomainsError> {
        Ok(())
    }
    async fn order_failed(&self, _h: &str, _d: &str) -> Result<(), DomainsError> {
        Ok(())
    }
    async fn domain_removed(&self, _h: &str, _r: &str) -> Result<(), DomainsError> {
        Ok(())
    }
}

struct Harness {
    tx: TxManager,
    domains: Arc<dyn CustomDomainRepository>,
    orders: Arc<dyn DomainOrderRepository>,
    certs: Arc<dyn CertificateRepository>,
    provider: Arc<dyn DnsProviderPort>,
    registrar: Arc<dyn DomainRegistrarPort>,
    ca: Arc<dyn CertificateAuthorityPort>,
    verifier: FakeVerifier,
    events: Arc<dyn EventPublisher>,
    clock: Arc<FakeClock>,
    config: DomainsConfig,
}

/// A registrar that quotes normally but always fails `register` — the
/// "charged, then the registrar breaks" saga corner.
struct RegisterFailsRegistrar {
    inner: SandboxRegistrar,
}

#[async_trait::async_trait]
impl DomainRegistrarPort for RegisterFailsRegistrar {
    fn registrar_id(&self) -> String {
        self.inner.registrar_id()
    }

    fn catalog(&self) -> Vec<TldPrice> {
        self.inner.catalog()
    }

    async fn search(&self, query: &str) -> Result<Vec<DomainQuote>, DomainsError> {
        self.inner.search(query).await
    }

    async fn quote(
        &self,
        hostname: &Hostname,
        term_years: u8,
    ) -> Result<DomainQuote, DomainsError> {
        self.inner.quote(hostname, term_years).await
    }

    async fn register(
        &self,
        _order_id: Uuid,
        _hostname: &Hostname,
        _term_years: u8,
        _whois_privacy: bool,
    ) -> Result<RegistrationResult, DomainsError> {
        Err(DomainsError::Registrar(
            "simulated registrar outage".to_string(),
        ))
    }

    async fn renew(
        &self,
        external_ref: &str,
        hostname: &Hostname,
        term_years: u8,
    ) -> Result<RenewalResult, DomainsError> {
        self.inner.renew(external_ref, hostname, term_years).await
    }
}

/// A CA whose issuance can be switched off mid-test.
struct FlakyCa {
    inner: SandboxCertificateAuthority,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl CertificateAuthorityPort for FlakyCa {
    async fn issue(
        &self,
        domain: &CustomDomain,
    ) -> Result<sanvi_domains::domain::Certificate, DomainsError> {
        if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(DomainsError::CertificateAuthority(
                "simulated CA outage".to_string(),
            ));
        }
        self.inner.issue(domain).await
    }

    async fn renew(
        &self,
        cert: &sanvi_domains::domain::Certificate,
        domain: &CustomDomain,
    ) -> Result<sanvi_domains::domain::Certificate, DomainsError> {
        self.inner.renew(cert, domain).await
    }

    async fn revoke(&self, cert: &sanvi_domains::domain::Certificate) -> Result<(), DomainsError> {
        self.inner.revoke(cert).await
    }
}

fn test_config() -> DomainsConfig {
    DomainsConfig {
        platform_domain: "platform.sanvi.app".to_string(),
        edge_hostname: "edge.sanvi.app".to_string(),
        ..DomainsConfig::default()
    }
}

fn build_harness(
    pool: sanvi_persistence::PgPool,
    verifier: FakeVerifier,
    _prober: FakeProber,
    clock: Arc<FakeClock>,
) -> Harness {
    let tx = TxManager::new(pool.clone());
    let domains: Arc<dyn CustomDomainRepository> = Arc::new(SqlxCustomDomainRepository::new());
    let orders: Arc<dyn DomainOrderRepository> = Arc::new(SqlxDomainOrderRepository::new());
    let certs: Arc<dyn CertificateRepository> = Arc::new(SqlxCertificateRepository::new());
    let records = Arc::new(SqlxDnsRecordRepository::new());
    let provider: Arc<dyn DnsProviderPort> = Arc::new(SqlxDnsProvider::new(records));
    let registrar: Arc<dyn DomainRegistrarPort> = Arc::new(SandboxRegistrar::new(
        "sandbox".to_string(),
        &DomainsConfig::default().registrar.tlds,
    ));
    let ca: Arc<dyn CertificateAuthorityPort> = Arc::new(SandboxCertificateAuthority::new(90));
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    Harness {
        tx,
        domains,
        orders,
        certs,
        provider,
        registrar,
        ca,
        verifier,
        events,
        clock,
        config: test_config(),
    }
}

fn claim_use_case(harness: &Harness, gate: Arc<FakeGate>) -> Arc<ClaimCustomDomain> {
    Arc::new(ClaimCustomDomain::new(
        harness.domains.clone(),
        gate,
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.platform_domain.clone(),
        harness.config.connect_entitlement_key.clone(),
        harness.config.verification_timeout_hours,
        harness.config.verification_backoff_base_secs,
    ))
}

fn verification_sweep(harness: &Harness) -> Arc<RunVerificationSweep> {
    Arc::new(RunVerificationSweep::new(
        harness.domains.clone(),
        Arc::new(harness.verifier.clone()),
        harness.provider.clone(),
        harness.events.clone(),
        harness.tx.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
        harness.config.verification_backoff_base_secs,
        harness.config.verification_backoff_max_secs,
    ))
}

fn cert_sweep(harness: &Harness) -> Arc<RunCertIssuanceSweep> {
    Arc::new(RunCertIssuanceSweep::new(
        harness.domains.clone(),
        harness.certs.clone(),
        harness.ca.clone(),
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
    ))
}

async fn claim(harness: &Harness, tenant_id: Uuid, hostname: &str, role: Option<&str>) -> Uuid {
    let gate = Arc::new(FakeGate { enabled: true });
    let use_case = claim_use_case(harness, gate);
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let view = use_case
        .execute(
            &mut scope,
            tenant_id,
            &sanvi_domains::application::ClaimCustomDomainCommand {
                hostname: hostname.to_string(),
                role: role.map(str::to_string),
            },
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();
    // Claims become eligible for a poll after the backoff window; advance
    // the fake clock past it.
    harness
        .clock
        .set(harness.clock.now() + Duration::seconds(30));
    view.id
}

async fn find_domain(
    harness: &Harness,
    tenant_id: Uuid,
    domain_id: Uuid,
) -> sanvi_domains::domain::CustomDomain {
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let domain = harness
        .domains
        .find(&mut scope, tenant_id, domain_id)
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    domain
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_journey_claims_verifies_issues_and_goes_live() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let verifier = FakeVerifier::new(true, true);
    verifier.nameservers(&["ada.ns.cloudflare.com"]);
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        verifier,
        FakeProber { ok: true },
        clock.clone(),
    );

    let domain_id = claim(&harness, tenant_id, "www.acme.com", Some("primary")).await;
    let domain = find_domain(&harness, tenant_id, domain_id).await;
    assert_eq!(domain.status, DomainStatus::PendingSetup);
    assert_eq!(domain.role, DomainRole::Primary);

    // The verification sweep checks ownership and routing independently.
    let sweep = verification_sweep(&harness);
    let report = sweep.execute(clock.now(), 10).await.unwrap();
    assert_eq!(report.verified, 1);

    let domain = find_domain(&harness, tenant_id, domain_id).await;
    assert_eq!(domain.status, DomainStatus::Verified);
    assert_eq!(domain.detected_registrar.as_deref(), Some("cloudflare"));

    // Certificate issuance → live.
    let cert_sweep = cert_sweep(&harness);
    let report = cert_sweep.execute(10).await.unwrap();
    assert_eq!(report.issued, 1);

    let domain = find_domain(&harness, tenant_id, domain_id).await;
    assert_eq!(domain.status, DomainStatus::Live);

    // The resolution lookup + TLS authorize gate agree.
    let lookup = sanvi_domains::application::LiveDomainLookupService::new(
        Arc::new(LiveDomainCache::new(None, 300)),
        harness.domains.clone(),
        pool.clone(),
    );
    assert_eq!(lookup.resolve("www.acme.com").await, Some(tenant_id));
    assert_eq!(lookup.resolve("other.example.com").await, None);

    let authorize = AuthorizeTls::new(
        harness.domains.clone(),
        Arc::new(FakeTenantState { active: true }),
        pool.clone(),
    );
    assert!(
        authorize
            .execute("www.acme.com")
            .await
            .unwrap()
            .is_allowed()
    );
}

#[tokio::test]
async fn tls_authorize_refuses_every_hostname_we_do_not_serve() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    let authorize = AuthorizeTls::new(
        harness.domains.clone(),
        Arc::new(FakeTenantState { active: true }),
        pool.clone(),
    );

    // Unknown host.
    assert!(
        !authorize
            .execute("never-claimed.com")
            .await
            .unwrap()
            .is_allowed()
    );
    // Claimed but unverified.
    let domain_id = claim(&harness, tenant_id, "unverified.example.com", None).await;
    assert!(
        !authorize
            .execute("unverified.example.com")
            .await
            .unwrap()
            .is_allowed()
    );
    // Invalid hostname.
    assert!(
        !authorize
            .execute("not a hostname")
            .await
            .unwrap()
            .is_allowed()
    );

    // Verified for an *inactive* tenant → denied.
    let sweep = verification_sweep(&harness);
    sweep.execute(clock.now(), 10).await.unwrap();
    let inactive = AuthorizeTls::new(
        harness.domains.clone(),
        Arc::new(FakeTenantState { active: false }),
        pool.clone(),
    );
    assert!(
        !inactive
            .execute("unverified.example.com")
            .await
            .unwrap()
            .is_allowed()
    );

    let _ = domain_id;
}

#[tokio::test]
async fn purchase_saga_ends_live_without_human_steps() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let charger = FakeCharger::default();
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    let place_order = Arc::new(PlaceDomainOrder::new(
        harness.orders.clone(),
        harness.domains.clone(),
        harness.registrar.clone(),
        harness.provider.clone(),
        Arc::new(charger.clone()),
        Arc::new(FakeGate { enabled: true }),
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.platform_domain.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
        harness.config.purchase_entitlement_key.clone(),
        harness.config.verification_timeout_hours,
        harness.config.verification_backoff_base_secs,
    ));

    let view = place_order
        .execute(
            tenant_id,
            None,
            &sanvi_domains::application::PlaceDomainOrderCommand {
                hostname: "acme-purchase.com".to_string(),
                term_years: 1,
                auto_renew: true,
                whois_privacy: true,
                registrant_contact: sanvi_domains::domain::RegistrantContact {
                    name: "Acme".to_string(),
                    email: "ops@acme.test".to_string(),
                    phone: "+81-3-0000-0000".to_string(),
                    organization: None,
                    country_code: "jp".to_string(),
                },
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(view.status, "active");
    assert!(!charger.charges.lock().unwrap().is_empty());

    // The claim exists and is verified from the provider's records (we
    // control the zone — no external DNS needed). Claims become pollable
    // after the backoff window.
    clock.set(clock.now() + Duration::seconds(30));
    let sweep = verification_sweep(&harness);
    let report = sweep.execute(clock.now(), 10).await.unwrap();
    assert_eq!(report.verified, 1);

    let cert_sweep = cert_sweep(&harness);
    let report = cert_sweep.execute(10).await.unwrap();
    assert_eq!(report.issued, 1);

    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let domains = harness.domains.list(&mut scope, tenant_id).await.unwrap();
    assert!(domains.iter().any(|domain| {
        domain.hostname.as_str() == "acme-purchase.com"
            && domain.status == DomainStatus::Live
            && domain.kind == sanvi_domains::domain::DomainKind::Purchased
    }));

    // Managed records exist: challenge TXT + apex A records.
    let records = harness
        .provider
        .records(&mut scope, domains[0].id)
        .await
        .unwrap();
    assert!(
        records
            .iter()
            .any(|record| record.record_type == DnsRecordType::Txt)
    );
    assert!(
        records
            .iter()
            .any(|record| record.record_type == DnsRecordType::A)
    );
    scope.commit().await.unwrap();

    // Idempotency: the same command returns the same order, no second charge.
    let charges_before = charger.charges.lock().unwrap().len();
    let view2 = place_order
        .execute(
            tenant_id,
            None,
            &sanvi_domains::application::PlaceDomainOrderCommand {
                hostname: "acme-purchase.com".to_string(),
                term_years: 1,
                auto_renew: true,
                whois_privacy: true,
                registrant_contact: sanvi_domains::domain::RegistrantContact {
                    name: "Acme".to_string(),
                    email: "ops@acme.test".to_string(),
                    phone: "+81-3-0000-0000".to_string(),
                    organization: None,
                    country_code: "jp".to_string(),
                },
                idempotency_key: None,
            },
        )
        .await;
    assert!(view2.is_err() || view2.unwrap().status == "active");
    assert_eq!(charger.charges.lock().unwrap().len(), charges_before);
}

#[tokio::test]
async fn first_verifier_wins_and_the_squatter_loses_cleanly() {
    let (pool, _node) = prepare().await;
    let tenant_a = seed_tenant(&pool, "a-corp").await;
    let tenant_b = seed_tenant(&pool, "b-corp").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    // A squats (claims without configuring DNS); B claims and configures
    // DNS. Push A's next check into the future so B verifies first.
    let a_id = claim(&harness, tenant_a, "contested.example.com", None).await;
    let b_id = claim(&harness, tenant_b, "contested.example.com", None).await;

    {
        let mut platform = harness
            .tx
            .begin_unscoped("domains.test.squat", "test")
            .await
            .unwrap();
        sqlx::query("UPDATE domains.custom_domains SET next_check_at = $1 WHERE id = $2")
            .bind(clock.now() + Duration::hours(1))
            .bind(a_id)
            .execute(&mut **platform.tx_mut().as_inner())
            .await
            .unwrap();
        platform.commit().await.unwrap();
    }

    let sweep = verification_sweep(&harness);
    sweep.execute(clock.now(), 10).await.unwrap();

    // B verified first: the live claim is B's.
    let b_domain = find_domain(&harness, tenant_b, b_id).await;
    assert_eq!(b_domain.status, DomainStatus::Verified);

    // When A's poll comes due, the partial unique index rejects the
    // verification: A's claim closes with the clear reason.
    clock.set(clock.now() + Duration::hours(1));
    sweep.execute(clock.now(), 10).await.unwrap();
    let a_domain = find_domain(&harness, tenant_a, a_id).await;
    // The verified claim wins; the loser is removed with the reason.
    assert_eq!(a_domain.status, DomainStatus::Removed);
    assert_eq!(
        a_domain
            .failure
            .as_ref()
            .map(|failure| failure.code.as_str()),
        Some("verified_by_other_tenant")
    );
}

#[tokio::test]
async fn failed_verification_backs_off_and_expired_claims_close() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(false, false),
        FakeProber { ok: true },
        clock.clone(),
    );

    let domain_id = claim(&harness, tenant_id, "slow.example.com", None).await;
    let sweep = verification_sweep(&harness);
    sweep.execute(clock.now(), 10).await.unwrap();

    let domain = find_domain(&harness, tenant_id, domain_id).await;
    assert_eq!(domain.status, DomainStatus::Verifying);
    assert!(domain.next_check_at.unwrap() > clock.now());
    assert!(domain.failure.is_some());

    // The manual verify endpoint throttles on the backoff schedule.
    let verify = Arc::new(RequestVerification::new(
        harness.domains.clone(),
        clock.clone(),
    ));
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let err = verify
        .execute(&mut scope, tenant_id, domain_id)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainsError::VerificationThrottled { .. }));
    scope.commit().await.unwrap();

    // 72h later: the claim expires and the hostname is free again.
    clock.set(clock.now() + Duration::hours(73));
    sweep.execute(clock.now(), 10).await.unwrap();
    let domain = find_domain(&harness, tenant_id, domain_id).await;
    assert_eq!(domain.status, DomainStatus::Removed);
    assert_eq!(
        domain.failure.as_ref().map(|failure| failure.code.as_str()),
        Some("challenge_expired")
    );
}

#[tokio::test]
async fn renewal_failure_escalates_and_expiry_warnings_fire() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    // Take a domain live first.
    let domain_id = claim(&harness, tenant_id, "renewal.example.com", None).await;
    verification_sweep(&harness)
        .execute(clock.now(), 10)
        .await
        .unwrap();
    cert_sweep(&harness).execute(10).await.unwrap();

    // Day 61 (T-29): the CA is failing. Renewal fails and escalates, and
    // the T-30 expiry warning fires (a simulated renewal failure pages
    // before expiry).
    clock.set(clock.now() + Duration::days(61));
    let failing_ca =
        Arc::new(SandboxCertificateAuthority::new(90).with_renewal_error("simulated CA outage"));
    let renewal = Arc::new(RunRenewalSweep::new(
        harness.domains.clone(),
        harness.certs.clone(),
        harness.orders.clone(),
        failing_ca,
        harness.registrar.clone(),
        Arc::new(FakeCharger::default()),
        harness.events.clone(),
        Arc::new(FakeNotifications),
        harness.tx.clone(),
        30,
        vec![30, 14, 7, 1],
        30,
        86400,
        3,
    ));
    let report = renewal.execute(clock.now(), 10).await.unwrap();
    assert_eq!(report.certs_renewed, 0);
    assert_eq!(report.cert_renewal_failures, 1);
    assert_eq!(report.expiry_warnings, 1);

    // The failure is recorded on the certificate and the domain stays up
    // (the old cert still serves; escalation continues every sweep).
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let cert = harness
        .certs
        .find_for_domain(&mut scope, tenant_id, domain_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        cert.last_error
            .as_deref()
            .unwrap_or("")
            .contains("simulated CA outage")
    );
    scope.commit().await.unwrap();

    let domain = find_domain(&harness, tenant_id, domain_id).await;
    assert_eq!(domain.status, DomainStatus::Live);
}

#[tokio::test]
async fn health_monitor_detects_dns_drift_and_recovers() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    let domain_id = claim(&harness, tenant_id, "drift.example.com", None).await;
    verification_sweep(&harness)
        .execute(clock.now(), 10)
        .await
        .unwrap();
    cert_sweep(&harness).execute(10).await.unwrap();

    let health = Arc::new(RunHealthSweep::new(
        harness.domains.clone(),
        harness.certs.clone(),
        Arc::new(FakeVerifier::new(true, true)),
        Arc::new(FakeProber { ok: true }),
        harness.events.clone(),
        Arc::new(FakeNotifications),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
    ));
    let report = health.execute(10).await.unwrap();
    assert_eq!(report.degraded, 0);

    // DNS drift: routing no longer points at us.
    let health_drift = Arc::new(RunHealthSweep::new(
        harness.domains.clone(),
        harness.certs.clone(),
        Arc::new(FakeVerifier::new(true, false)),
        Arc::new(FakeProber { ok: true }),
        harness.events.clone(),
        Arc::new(FakeNotifications),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
    ));
    let report = health_drift.execute(10).await.unwrap();
    assert_eq!(report.degraded, 1);
    let domain = find_domain(&harness, tenant_id, domain_id).await;
    assert_eq!(domain.status, DomainStatus::Degraded);
    assert_eq!(
        domain.failure.as_ref().map(|failure| failure.code.as_str()),
        Some("dns_drift")
    );

    // Records restored → healthy again.
    let report = health.execute(10).await.unwrap();
    assert_eq!(report.recovered, 1);
    let domain = find_domain(&harness, tenant_id, domain_id).await;
    assert_eq!(domain.status, DomainStatus::Live);
}

#[tokio::test]
async fn tenant_isolation_hides_other_tenants_domains() {
    let (pool, _node) = prepare().await;
    let tenant_a = seed_tenant(&pool, "a-corp").await;
    let tenant_b = seed_tenant(&pool, "b-corp").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    claim(&harness, tenant_a, "private-a.com", None).await;

    // A sees their domain; B sees nothing.
    let mut scope_a = harness.tx.begin_for(tenant_a).await.unwrap();
    assert_eq!(
        harness
            .domains
            .list(&mut scope_a, tenant_a)
            .await
            .unwrap()
            .len(),
        1
    );
    scope_a.commit().await.unwrap();

    let mut scope_b = harness.tx.begin_for(tenant_b).await.unwrap();
    assert!(
        harness
            .domains
            .list(&mut scope_b, tenant_b)
            .await
            .unwrap()
            .is_empty()
    );
    // B cannot read A's domain row either (RLS + scoped find).
    let a_id = {
        let mut scope_a = harness.tx.begin_for(tenant_a).await.unwrap();
        let id = harness.domains.list(&mut scope_a, tenant_a).await.unwrap()[0].id;
        scope_a.commit().await.unwrap();
        id
    };
    assert!(
        harness
            .domains
            .find(&mut scope_b, tenant_b, a_id)
            .await
            .unwrap()
            .is_none()
    );
    scope_b.commit().await.unwrap();
}

#[tokio::test]
async fn removal_revokes_the_certificate_and_closes_the_zone() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    // Purchase → live, then remove: cert revoked, zone rows deleted, and
    // the hostname no longer resolves.
    let place_order = Arc::new(PlaceDomainOrder::new(
        harness.orders.clone(),
        harness.domains.clone(),
        harness.registrar.clone(),
        harness.provider.clone(),
        Arc::new(FakeCharger::default()),
        Arc::new(FakeGate { enabled: true }),
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.platform_domain.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
        harness.config.purchase_entitlement_key.clone(),
        harness.config.verification_timeout_hours,
        harness.config.verification_backoff_base_secs,
    ));
    place_order
        .execute(
            tenant_id,
            None,
            &sanvi_domains::application::PlaceDomainOrderCommand {
                hostname: "removable.com".to_string(),
                term_years: 1,
                auto_renew: true,
                whois_privacy: true,
                registrant_contact: sanvi_domains::domain::RegistrantContact {
                    name: "Acme".to_string(),
                    email: "ops@acme.test".to_string(),
                    phone: "+81-3-0000-0000".to_string(),
                    organization: None,
                    country_code: "jp".to_string(),
                },
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    clock.set(clock.now() + Duration::seconds(30));
    verification_sweep(&harness)
        .execute(clock.now(), 10)
        .await
        .unwrap();
    cert_sweep(&harness).execute(10).await.unwrap();

    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let domain_id = harness
        .domains
        .list(&mut scope, tenant_id)
        .await
        .unwrap()
        .into_iter()
        .find(|domain| domain.hostname.as_str() == "removable.com")
        .unwrap()
        .id;

    let remove = Arc::new(RemoveCustomDomain::new(
        harness.domains.clone(),
        harness.certs.clone(),
        harness.provider.clone(),
        harness.ca.clone(),
        harness.events.clone(),
        harness.clock.clone(),
    ));
    let view = remove
        .execute(&mut scope, tenant_id, domain_id)
        .await
        .unwrap();
    assert_eq!(view.status, "removed");

    let cert = harness
        .certs
        .find_for_domain(&mut scope, tenant_id, domain_id)
        .await
        .unwrap();
    assert_eq!(
        cert.map(|cert| cert.status),
        Some(sanvi_domains::domain::CertificateStatus::Revoked)
    );
    assert!(
        harness
            .provider
            .records(&mut scope, domain_id)
            .await
            .unwrap()
            .is_empty()
    );
    scope.commit().await.unwrap();

    // The lookup no longer resolves the hostname.
    let lookup = sanvi_domains::application::LiveDomainLookupService::new(
        Arc::new(LiveDomainCache::new(None, 300)),
        harness.domains.clone(),
        pool.clone(),
    );
    assert_eq!(lookup.resolve("removable.com").await, None);
}

#[tokio::test]
async fn primary_promotion_demotes_the_previous_primary() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    let first = claim(&harness, tenant_id, "first.com", Some("primary")).await;
    let second = claim(&harness, tenant_id, "second.com", Some("alias")).await;

    let promote = Arc::new(PromotePrimaryDomain::new(
        harness.domains.clone(),
        harness.events.clone(),
        harness.clock.clone(),
    ));
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let view = promote
        .execute(&mut scope, tenant_id, second)
        .await
        .unwrap();
    assert_eq!(view.role, "primary");
    let demoted = harness
        .domains
        .find(&mut scope, tenant_id, first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(demoted.role, DomainRole::Alias);
    scope.commit().await.unwrap();
}

#[tokio::test]
async fn auto_renew_toggle_updates_the_order() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    let place_order = Arc::new(PlaceDomainOrder::new(
        harness.orders.clone(),
        harness.domains.clone(),
        harness.registrar.clone(),
        harness.provider.clone(),
        Arc::new(FakeCharger::default()),
        Arc::new(FakeGate { enabled: true }),
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.platform_domain.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
        harness.config.purchase_entitlement_key.clone(),
        harness.config.verification_timeout_hours,
        harness.config.verification_backoff_base_secs,
    ));
    let view = place_order
        .execute(
            tenant_id,
            None,
            &sanvi_domains::application::PlaceDomainOrderCommand {
                hostname: "autorenew.com".to_string(),
                term_years: 1,
                auto_renew: true,
                whois_privacy: true,
                registrant_contact: sanvi_domains::domain::RegistrantContact {
                    name: "Acme".to_string(),
                    email: "ops@acme.test".to_string(),
                    phone: "+81-3-0000-0000".to_string(),
                    organization: None,
                    country_code: "jp".to_string(),
                },
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    let order_id = view.id;

    let toggle = Arc::new(SetOrderAutoRenew::new(
        harness.orders.clone(),
        harness.clock.clone(),
        false,
    ));
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let view = toggle
        .execute(&mut scope, tenant_id, order_id)
        .await
        .unwrap();
    assert!(!view.auto_renew);
    scope.commit().await.unwrap();
}

// ---------------------------------------------------------------------------
// Saga-safety regressions.
// ---------------------------------------------------------------------------

fn place_order_command(hostname: &str) -> sanvi_domains::application::PlaceDomainOrderCommand {
    sanvi_domains::application::PlaceDomainOrderCommand {
        hostname: hostname.to_string(),
        term_years: 1,
        auto_renew: true,
        whois_privacy: true,
        registrant_contact: sanvi_domains::domain::RegistrantContact {
            name: "Acme".to_string(),
            email: "ops@acme.test".to_string(),
            phone: "+81-3-0000-0000".to_string(),
            organization: None,
            country_code: "jp".to_string(),
        },
        idempotency_key: None,
    }
}

#[tokio::test]
async fn failed_refund_is_retried_by_the_order_sweep_until_it_lands() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let charger = FakeCharger {
        fail_refund: true,
        ..FakeCharger::default()
    };
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    // The registrar breaks after the charge, and the refund call breaks
    // too: the order must close as `failed` carrying the `refund_error`
    // marker the order sweep keys its retries on.
    let place_order = Arc::new(PlaceDomainOrder::new(
        harness.orders.clone(),
        harness.domains.clone(),
        Arc::new(RegisterFailsRegistrar {
            inner: SandboxRegistrar::new(
                "sandbox".to_string(),
                &DomainsConfig::default().registrar.tlds,
            ),
        }),
        harness.provider.clone(),
        Arc::new(charger.clone()),
        Arc::new(FakeGate { enabled: true }),
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.platform_domain.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
        harness.config.purchase_entitlement_key.clone(),
        harness.config.verification_timeout_hours,
        harness.config.verification_backoff_base_secs,
    ));
    let view = place_order
        .execute(tenant_id, None, &place_order_command("refund-retry.com"))
        .await
        .unwrap();
    assert_eq!(view.status, "failed");

    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let order = harness
        .orders
        .find(&mut scope, tenant_id, view.id)
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    assert!(
        order
            .failure_detail
            .as_ref()
            .and_then(|detail| detail.get("refund_error"))
            .is_some(),
        "the failed refund must be recorded for the sweep to retry"
    );

    // The sweep retries the refund with a healthy charger: the order lands
    // in `refunded`, money resolved.
    let healthy_place_order = Arc::new(PlaceDomainOrder::new(
        harness.orders.clone(),
        harness.domains.clone(),
        harness.registrar.clone(),
        harness.provider.clone(),
        Arc::new(FakeCharger::default()),
        Arc::new(FakeGate { enabled: true }),
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.platform_domain.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
        harness.config.purchase_entitlement_key.clone(),
        harness.config.verification_timeout_hours,
        harness.config.verification_backoff_base_secs,
    ));
    let sweep = Arc::new(RunOrderSweep::new(
        harness.orders.clone(),
        harness.registrar.clone(),
        Arc::new(FakeCharger::default()),
        healthy_place_order,
        harness.tx.clone(),
    ));
    let report = sweep.execute(10).await.unwrap();
    assert_eq!(report.refunded, 1);

    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let order = harness
        .orders
        .find(&mut scope, tenant_id, view.id)
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    assert_eq!(order.status, sanvi_domains::domain::OrderStatus::Refunded);
}

#[tokio::test]
async fn certificate_issuance_failure_is_retried_on_the_next_sweep() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    let domain_id = claim(&harness, tenant_id, "flaky-ca.com", None).await;
    verification_sweep(&harness)
        .execute(clock.now(), 10)
        .await
        .unwrap();

    // The CA is down: the domain moves to issuing_cert and must not be
    // stranded there when the query only looks for `verified` rows.
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flaky_sweep = Arc::new(RunCertIssuanceSweep::new(
        harness.domains.clone(),
        harness.certs.clone(),
        Arc::new(FlakyCa {
            inner: SandboxCertificateAuthority::new(90),
            fail: fail.clone(),
        }),
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
    ));
    let report = flaky_sweep.execute(10).await.unwrap();
    assert_eq!(report.failures, 1);
    assert_eq!(
        find_domain(&harness, tenant_id, domain_id).await.status,
        DomainStatus::IssuingCert
    );

    // CA recovered: the stranded issuance is retried and goes live.
    fail.store(false, std::sync::atomic::Ordering::Relaxed);
    let report = flaky_sweep.execute(10).await.unwrap();
    assert_eq!(report.issued, 1);
    assert_eq!(
        find_domain(&harness, tenant_id, domain_id).await.status,
        DomainStatus::Live
    );
}

#[tokio::test]
async fn purchasing_a_hostname_the_tenant_already_claimed_supersedes_the_connect_claim() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let harness = build_harness(
        pool.clone(),
        FakeVerifier::new(true, true),
        FakeProber { ok: true },
        clock.clone(),
    );

    // The tenant first tries the connect journey (claim, never verified),
    // then buys the same hostname through Sanvi.
    let connect_id = claim(&harness, tenant_id, "dual-journey.com", None).await;
    assert_eq!(
        find_domain(&harness, tenant_id, connect_id).await.role,
        DomainRole::Primary
    );

    let place_order = Arc::new(PlaceDomainOrder::new(
        harness.orders.clone(),
        harness.domains.clone(),
        harness.registrar.clone(),
        harness.provider.clone(),
        Arc::new(FakeCharger::default()),
        Arc::new(FakeGate { enabled: true }),
        harness.events.clone(),
        harness.clock.clone(),
        harness.tx.clone(),
        harness.config.platform_domain.clone(),
        harness.config.edge_hostname.clone(),
        harness.config.anycast_ips.clone(),
        harness.config.purchase_entitlement_key.clone(),
        harness.config.verification_timeout_hours,
        harness.config.verification_backoff_base_secs,
    ));
    let view = place_order
        .execute(tenant_id, None, &place_order_command("dual-journey.com"))
        .await
        .unwrap();
    assert_eq!(view.status, "active");

    // The connect claim closed with the supersession reason; the purchased
    // claim inherited the primary role.
    let connect_claim = find_domain(&harness, tenant_id, connect_id).await;
    assert_eq!(connect_claim.status, DomainStatus::Removed);
    assert_eq!(
        connect_claim
            .failure
            .as_ref()
            .map(|failure| failure.code.as_str()),
        Some("superseded_by_purchase")
    );

    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let domains = harness.domains.list(&mut scope, tenant_id).await.unwrap();
    scope.commit().await.unwrap();
    let purchased = domains
        .iter()
        .find(|domain| {
            domain.hostname.as_str() == "dual-journey.com"
                && domain.kind == sanvi_domains::domain::DomainKind::Purchased
        })
        .expect("the purchased claim exists");
    assert_eq!(purchased.role, DomainRole::Primary);

    // And the purchased claim verifies from the provider's records — the
    // challenge token and the applied TXT record must agree.
    clock.set(clock.now() + Duration::seconds(30));
    let report = verification_sweep(&harness)
        .execute(clock.now(), 10)
        .await
        .unwrap();
    assert_eq!(report.verified, 1);
}
