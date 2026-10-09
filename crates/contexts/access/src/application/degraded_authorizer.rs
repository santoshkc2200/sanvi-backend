//! The degraded authorizer (TASK-023, FR-1113): what authorization does
//! while Keto is down.
//!
//! **Writes deny by default; reads serve the cached decision for at most
//! the grace window (30 s in production — the decision cache's own TTL),
//! then deny.** Access is never granted "because the backend was down",
//! and the failure is never surfaced as a `503` retry invite: a deny is
//! cheap for the caller to retry, a wrong allow is unbounded damage.
//! While degraded, the registry carries `keto` so the response header can
//! say so honestly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sanvi_kernel::UserId;
use sanvi_resilience::DegradationRegistry;
use tokio::sync::Mutex;

use crate::domain::ports::{Authorizer, Decision, DecisionCache, ResourceRef};
use crate::domain::{AccessError, Permission};

use super::check_permission::decision_cache_key;

/// The production grace window: the decision cache TTL. A cached read is
/// never older than this, so the window bounds exactly how stale a
/// decision an outage may serve.
pub const KETO_DOWNGRADE_GRACE_SECS: u64 = 30;

pub struct DegradedAuthorizer {
    inner: Arc<dyn Authorizer>,
    cache: Arc<dyn DecisionCache>,
    degradation: Arc<DegradationRegistry>,
    grace: Duration,
    outage: Mutex<Option<Instant>>,
}

impl DegradedAuthorizer {
    /// The production assembly: 30 s grace, the shared registry.
    pub fn new(
        inner: Arc<dyn Authorizer>,
        cache: Arc<dyn DecisionCache>,
        degradation: Arc<DegradationRegistry>,
    ) -> Self {
        Self::with_grace(
            inner,
            cache,
            degradation,
            Duration::from_secs(KETO_DOWNGRADE_GRACE_SECS),
        )
    }

    /// Test constructor: a shortened grace window so the post-grace deny
    /// is observable in milliseconds.
    pub fn with_grace(
        inner: Arc<dyn Authorizer>,
        cache: Arc<dyn DecisionCache>,
        degradation: Arc<DegradationRegistry>,
        grace: Duration,
    ) -> Self {
        Self {
            inner,
            cache,
            degradation,
            grace,
            outage: Mutex::new(None),
        }
    }

    /// The check every caller runs. `write` distinguishes mutations (deny
    /// by default while Keto is down) from reads (cached decision within
    /// the grace window, then deny).
    pub async fn check_with_mode(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
        write: bool,
    ) -> Result<Decision, AccessError> {
        // A cache hit cannot prove that Keto has recovered, and cannot
        // authorize a mutation without contacting the backend.
        let result = if write
            || self
                .degradation
                .is_active(sanvi_resilience::DegradationReason::Keto)
        {
            self.inner.check_fresh(user_id, permission, resource).await
        } else {
            self.inner
                .check(user_id, permission, resource)
                .await
                .map(|decision| (decision, false))
        };
        match result {
            Ok((decision, verified)) => {
                if verified {
                    self.recovered().await;
                } else if write
                    && self
                        .degradation
                        .is_active(sanvi_resilience::DegradationReason::Keto)
                {
                    return Ok(Decision::Denied);
                }
                return Ok(decision);
            }
            // Storage errors are Postgres's row of the matrix, not Keto's:
            // they propagate as errors (the HTTP gate renders them 503).
            Err(err) if !is_keto_failure(&err) => return Err(err),
            Err(err) => {
                tracing::warn!(error = %err, write, "keto unavailable; degrading");
            }
        }

        let mut outage = self.outage.lock().await;
        let since = *outage.get_or_insert(Instant::now());
        drop(outage);
        self.degradation
            .set(sanvi_resilience::DegradationReason::Keto, true);

        // Writes deny while degraded, whatever the window says.
        if write {
            return Ok(Decision::Denied);
        }

        // Reads: the cached decision within the grace window, deny after.
        if since.elapsed() <= self.grace {
            let key = decision_cache_key(user_id, permission.as_str(), resource);
            if let Some(decision) = self.cache.get(&key).await {
                return Ok(decision);
            }
        }
        Ok(Decision::Denied)
    }

    async fn recovered(&self) {
        let mut outage = self.outage.lock().await;
        if outage.take().is_some() {
            tracing::info!("keto recovered; clearing degradation");
        }
        drop(outage);
        self.degradation
            .set(sanvi_resilience::DegradationReason::Keto, false);
    }
}

/// The errors that mean "Keto itself is unavailable" — transport failures
/// and rejected calls — as opposed to storage trouble inside the check's
/// role lookup.
fn is_keto_failure(err: &AccessError) -> bool {
    matches!(
        err,
        AccessError::BackendUnreachable(_) | AccessError::BackendRejected(_)
    )
}

#[async_trait::async_trait]
impl Authorizer for DegradedAuthorizer {
    async fn check(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<Decision, AccessError> {
        // Callers without HTTP method context may mutate state. Require a
        // fresh decision; the HTTP gate explicitly opts reads into grace.
        self.check_with_mode(user_id, permission, resource, true)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ports::PermissionStore;
    use std::collections::HashMap;

    #[tokio::test]
    async fn cached_allow_cannot_authorize_a_write_or_clear_a_known_outage() {
        struct CachedAllow;
        #[async_trait::async_trait]
        impl Authorizer for CachedAllow {
            async fn check(
                &self,
                _: UserId,
                _: &Permission,
                _: &ResourceRef,
            ) -> Result<Decision, AccessError> {
                Ok(Decision::Allowed)
            }
            async fn check_fresh(
                &self,
                _: UserId,
                _: &Permission,
                _: &ResourceRef,
            ) -> Result<(Decision, bool), AccessError> {
                Err(AccessError::BackendUnreachable("down".into()))
            }
        }
        let registry = Arc::new(DegradationRegistry::default());
        let authorizer = DegradedAuthorizer::new(
            Arc::new(CachedAllow),
            Arc::new(FakeDecisionCache::new()),
            registry.clone(),
        );
        assert_eq!(
            authorizer
                .check_with_mode(subject(), &permission(), &resource(), true)
                .await
                .unwrap(),
            Decision::Denied
        );
        assert!(registry.is_active(sanvi_resilience::DegradationReason::Keto));
        // A cached allow for another key is not evidence of recovery.
        assert_eq!(
            authorizer
                .check_with_mode(subject(), &permission(), &resource(), false)
                .await
                .unwrap(),
            Decision::Denied
        );
        assert!(registry.is_active(sanvi_resilience::DegradationReason::Keto));
    }

    struct FailingAuthorizer {
        err: AccessError,
    }

    #[async_trait::async_trait]
    impl Authorizer for FailingAuthorizer {
        async fn check(
            &self,
            _user_id: UserId,
            _permission: &Permission,
            _resource: &ResourceRef,
        ) -> Result<Decision, AccessError> {
            Err(self.err.clone())
        }
    }

    /// Port fake: the application layer must not reach into adapters,
    /// even in tests (layering gate) — so the cache double lives here.
    struct FakeDecisionCache {
        entries: tokio::sync::Mutex<HashMap<String, Decision>>,
    }

    impl FakeDecisionCache {
        fn new() -> Self {
            Self {
                entries: tokio::sync::Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl DecisionCache for FakeDecisionCache {
        async fn get(&self, key: &str) -> Option<Decision> {
            self.entries.lock().await.get(key).cloned()
        }

        async fn put(&self, key: &str, decision: Decision, _ttl_secs: u64) {
            self.entries.lock().await.insert(key.to_string(), decision);
        }

        async fn invalidate_user(&self, _user_id: UserId) {}

        async fn invalidate_all(&self) {
            self.entries.lock().await.clear();
        }
    }

    fn subject() -> UserId {
        UserId::try_from(1_000_042_i64).unwrap()
    }

    fn permission() -> Permission {
        Permission::parse("billing.subscriptions.read").unwrap()
    }

    fn resource() -> ResourceRef {
        ResourceRef::Tenant(sanvi_kernel::TenantId::try_from(1_000_001_i64).unwrap())
    }

    async fn degraded(err: AccessError) -> (DegradedAuthorizer, Arc<DegradationRegistry>) {
        let degradation = Arc::new(DegradationRegistry::default());
        let authorizer = DegradedAuthorizer::with_grace(
            Arc::new(FailingAuthorizer { err }),
            Arc::new(FakeDecisionCache::new()),
            degradation.clone(),
            Duration::from_millis(80),
        );
        (authorizer, degradation)
    }

    #[tokio::test]
    async fn writes_deny_and_reads_stay_cached_within_the_grace_window() {
        let key = decision_cache_key(subject(), permission().as_str(), &resource());
        let (authorizer, degradation) =
            degraded(AccessError::BackendUnreachable("down".to_string())).await;
        authorizer.cache.put(&key, Decision::Allowed, 30).await;

        // Write: denied.
        let outcome = authorizer
            .check_with_mode(subject(), &permission(), &resource(), true)
            .await
            .unwrap();
        assert_eq!(outcome, Decision::Denied);
        // Cached read: served.
        let outcome = authorizer
            .check_with_mode(subject(), &permission(), &resource(), false)
            .await
            .unwrap();
        assert_eq!(outcome, Decision::Allowed);
        // Uncached read: denied.
        let other = Permission::parse("billing.subscriptions.manage").unwrap();
        let outcome = authorizer
            .check_with_mode(subject(), &other, &resource(), false)
            .await
            .unwrap();
        assert_eq!(outcome, Decision::Denied);
        // Registry carries keto.
        assert_eq!(degradation.header_value().as_deref(), Some("keto"));
    }
    #[tokio::test]
    async fn after_the_grace_window_cached_reads_deny_too() {
        let key = decision_cache_key(subject(), permission().as_str(), &resource());
        let (authorizer, _) = degraded(AccessError::BackendUnreachable("down".to_string())).await;
        authorizer.cache.put(&key, Decision::Allowed, 30).await;
        // The first failure starts the outage clock; the cached read
        // serves within the grace window.
        let outcome = authorizer
            .check_with_mode(subject(), &permission(), &resource(), false)
            .await
            .unwrap();
        assert_eq!(outcome, Decision::Allowed);
        // Past the window the same cached decision denies.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let outcome = authorizer
            .check_with_mode(subject(), &permission(), &resource(), false)
            .await
            .unwrap();
        assert_eq!(outcome, Decision::Denied);
    }

    #[tokio::test]
    async fn storage_errors_propagate_and_never_flip_the_registry() {
        let degradation = Arc::new(DegradationRegistry::default());
        let authorizer = DegradedAuthorizer::with_grace(
            Arc::new(FailingAuthorizer {
                err: AccessError::Storage("pool exhausted".to_string()),
            }),
            Arc::new(FakeDecisionCache::new()),
            degradation.clone(),
            Duration::from_secs(30),
        );
        let outcome = authorizer
            .check_with_mode(subject(), &permission(), &resource(), false)
            .await;
        assert!(matches!(outcome, Err(AccessError::Storage(_))));
        assert_eq!(degradation.header_value(), None);
    }

    #[tokio::test]
    async fn recovery_clears_the_registry() {
        let (authorizer, degradation) = {
            struct Healthy;
            #[async_trait::async_trait]
            impl Authorizer for Healthy {
                async fn check(
                    &self,
                    _: UserId,
                    _: &Permission,
                    _: &ResourceRef,
                ) -> Result<Decision, AccessError> {
                    Ok(Decision::Allowed)
                }
            }
            let degradation = Arc::new(DegradationRegistry::default());
            let authorizer = DegradedAuthorizer::with_grace(
                Arc::new(Healthy),
                Arc::new(FakeDecisionCache::new()),
                degradation.clone(),
                Duration::from_secs(30),
            );
            (authorizer, degradation)
        };
        // Flip it on manually (as an earlier outage would have).
        degradation.set(sanvi_resilience::DegradationReason::Keto, true);
        let outcome = authorizer
            .check_with_mode(subject(), &permission(), &resource(), false)
            .await
            .unwrap();
        assert_eq!(outcome, Decision::Allowed);
        assert_eq!(degradation.header_value(), None);
    }

    /// Compile-time pin: `PermissionStore` stays object-safe alongside the
    /// authorizer surface this module wraps.
    const _: Option<&dyn PermissionStore> = None;
}
