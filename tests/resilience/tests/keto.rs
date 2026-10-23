//! Dependency-matrix injection tests: Keto (TASK-023, FR-1112/FR-1113).
//!
//! The degradation rule under injection: **Keto down denies writes by
//! default and serves cached reads for at most the grace window (30 s in
//! production), then denies** — access is never granted because the
//! backend was down. The failing authorizer is the port-level injection
//! that the HTTP-level injections below produce for real.

mod common;

use common::{FaultMode, FaultServer};
use sanvi_access::adapters::decision_cache::InMemoryDecisionCache;
use sanvi_access::adapters::keto::HttpKetoClient;
use sanvi_access::application::degraded_authorizer::DegradedAuthorizer;
use sanvi_access::domain::{AccessError, Permission};
use sanvi_access::domain::{Authorizer, Decision, DecisionCache, PermissionStore, ResourceRef};
use sanvi_kernel::UserId;
use std::sync::Arc;
use std::time::Duration;

/// An authorizer that fails the way a broken Keto does — the port-level
/// shape of the HTTP injections underneath.
struct FailingAuthorizer {
    err: AccessError,
}

#[async_trait::async_trait]
impl Authorizer for FailingAuthorizer {
    async fn cache_revision(
        &self,
        _: UserId,
        _: &Permission,
        _: &ResourceRef,
    ) -> Result<Option<String>, AccessError> {
        // This fixture models an unchanged authoritative SQL membership even
        // while the independent Keto transport is unavailable.
        Ok(Some("unchanged-fixture".into()))
    }
    async fn check(
        &self,
        _user_id: UserId,
        _permission: &Permission,
        _resource: &ResourceRef,
    ) -> Result<Decision, AccessError> {
        Err(self.err.clone())
    }
}

fn subject() -> UserId {
    UserId::try_from(1_000_042_i64).unwrap()
}

fn read_permission() -> Permission {
    Permission::parse("billing.subscriptions.read").expect("registered permission")
}

fn write_permission() -> Permission {
    Permission::parse("billing.subscriptions.manage").expect("registered permission")
}

fn tenant_resource() -> ResourceRef {
    ResourceRef::Tenant(sanvi_kernel::TenantId::try_from(1_000_001_i64).unwrap())
}

async fn primed_cache(key: &str, decision: Decision) -> Arc<dyn DecisionCache> {
    let cache: Arc<dyn DecisionCache> = Arc::new(InMemoryDecisionCache::new(1024));
    cache.put(key, decision, 30).await;
    cache
}

/// Keto · down — writes deny, cached reads serve within the grace window,
/// then deny. The registry carries `keto` for the degraded header.
#[tokio::test]
async fn keto_down_denies_writes_and_serves_cached_reads_for_the_grace_window() {
    let degradation = Arc::new(sanvi_resilience::DegradationRegistry::default());
    let cache = primed_cache(
        &format!(
            "{}:{}:tenant:{}:authority:unchanged-fixture",
            subject(),
            read_permission().as_str(),
            1_000_001
        ),
        Decision::Allowed,
    )
    .await;
    let authorizer = DegradedAuthorizer::with_grace(
        Arc::new(FailingAuthorizer {
            err: AccessError::BackendUnreachable("connection refused".to_string()),
        }),
        cache,
        degradation.clone(),
        Duration::from_millis(120),
    );

    // A write is denied by default while Keto is down — never 503'd into a
    // client retry storm, never allowed on faith.
    let outcome = authorizer
        .check_with_mode(subject(), &write_permission(), &tenant_resource(), true)
        .await
        .expect("degraded Keto denies, it does not error");
    assert_eq!(outcome, Decision::Denied);

    // A read whose decision is cached is served within the grace window.
    let outcome = authorizer
        .check_with_mode(subject(), &read_permission(), &tenant_resource(), false)
        .await
        .expect("cached read serves within grace");
    assert_eq!(outcome, Decision::Allowed);

    // A read with no cached decision denies (fail closed).
    let outcome = authorizer
        .check_with_mode(subject(), &write_permission(), &tenant_resource(), false)
        .await
        .expect("uncached read denies under degradation");
    assert_eq!(outcome, Decision::Denied);

    // After the grace window even the cached read denies.
    tokio::time::sleep(Duration::from_millis(180)).await;
    let outcome = authorizer
        .check_with_mode(subject(), &read_permission(), &tenant_resource(), false)
        .await
        .expect("post-grace read denies");
    assert_eq!(outcome, Decision::Denied);

    // The degraded flag is on for the header middleware.
    assert_eq!(degradation.header_value().as_deref(), Some("keto"));
}

/// Keto · erroring — a 500ing Keto takes the identical degraded path, and
/// the HTTP client surfaces its failures as backend errors the degraded
/// authorizer recognises.
#[tokio::test]
async fn keto_erroring_takes_the_same_degraded_path() {
    // The real client against a 500-answerer.
    let server = FaultServer::start(FaultMode::Status(500));
    let client = HttpKetoClient::with_timeout(
        server.base_url().to_string(),
        server.base_url().to_string(),
        Duration::from_millis(300),
    )
    .expect("client builds");
    let err = client
        .check_membership(
            UserId::try_from(1_i64).unwrap(),
            &["1000001:tenant_admin".to_string()],
        )
        .await
        .expect_err("a 500ing Keto cannot answer membership checks");
    assert!(
        matches!(err, AccessError::BackendRejected(_)),
        "expected BackendRejected, got: {err}"
    );

    // And the degraded authorizer treats that error exactly like down.
    let degradation = Arc::new(sanvi_resilience::DegradationRegistry::default());
    let authorizer = DegradedAuthorizer::with_grace(
        Arc::new(FailingAuthorizer {
            err: AccessError::BackendRejected("500".to_string()),
        }),
        Arc::new(InMemoryDecisionCache::new(1024)),
        degradation.clone(),
        Duration::from_secs(30),
    );
    let outcome = authorizer
        .check_with_mode(subject(), &write_permission(), &tenant_resource(), true)
        .await
        .expect("degraded Keto denies, it does not error");
    assert_eq!(outcome, Decision::Denied);
    assert_eq!(degradation.header_value().as_deref(), Some("keto"));
}

/// Keto · down (transport) — the real client against a closed port fails
/// with the unreachable error the degraded path recognises.
#[tokio::test]
async fn keto_transport_down_reports_backend_unreachable() {
    let (base, _port) = common::closed_endpoint();
    let client = HttpKetoClient::with_timeout(base.clone(), base, Duration::from_millis(300))
        .expect("client builds");
    let err = client
        .check_membership(
            UserId::try_from(1_i64).unwrap(),
            &["1000001:tenant_admin".to_string()],
        )
        .await
        .expect_err("a dead Keto cannot answer");
    assert!(
        matches!(err, AccessError::BackendUnreachable(_)),
        "got: {err}"
    );
}
