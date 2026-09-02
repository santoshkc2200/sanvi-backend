//! Per-connection request budgeting (TASK-012, 10.3).
//!
//! Google Ads quota is per developer token and shared across every tenant on
//! this platform; Meta's app-level rate limit is likewise shared. Both are a
//! single scarce resource one tenant's bulk edit could otherwise exhaust for
//! everyone else. [`RateLimitedAdapter`] queues and caps in-flight calls per
//! connection *and* platform-wide, so a burst from one connection waits
//! instead of starving another tenant's sync.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::domain::{
    AdPlatformPort, AdvertisingError, AuthorizationExchange, AuthorizationRequest,
    AuthorizationStart, Campaign, ConnectionHandle, ConsentSnapshot, ExchangedAuthorization,
    HashedIds, MetricPoint, MetricQuery, PlatformAccount, PlatformDescriptor, UploadedConversion,
};

/// Ceiling on distinct keys held at once. Connection ids are bounded by
/// how many connections a tenant owns, but the public track beacon keys
/// this limiter by client IP and `Origin` — both chosen by an
/// unauthenticated caller — so the table must be bounded, or N distinct
/// values permanently allocate N semaphores.
const MAX_TRACKED_KEYS: usize = 4096;

/// Bounds concurrent adapter calls two ways at once: a platform-wide cap
/// (the shared developer-token quota) and a per-connection cap nested
/// inside it (so one connection cannot claim every platform-wide slot).
/// A caller always holds both permits or neither.
pub struct QuotaLimiter {
    platform_wide: Arc<Semaphore>,
    per_connection_permits: usize,
    connections: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// Shared bucket for keys that arrive while every tracked key is busy.
    /// Coarser than a key of one's own, but bounded — and still a real
    /// budget, so an overflowing caller is throttled rather than refused.
    overflow: Arc<Semaphore>,
}

impl QuotaLimiter {
    pub fn new(platform_wide_permits: usize, per_connection_permits: usize) -> Self {
        Self {
            platform_wide: Arc::new(Semaphore::new(platform_wide_permits.max(1))),
            per_connection_permits: per_connection_permits.max(1),
            connections: Mutex::new(HashMap::new()),
            overflow: Arc::new(Semaphore::new(per_connection_permits.max(1))),
        }
    }

    fn connection_semaphore(&self, connection_id: &str) -> Arc<Semaphore> {
        let mut connections = self
            .connections
            .lock()
            .expect("advertising quota limiter mutex poisoned");

        if let Some(existing) = connections.get(connection_id) {
            return existing.clone();
        }

        if connections.len() >= MAX_TRACKED_KEYS {
            // A semaphore the map alone still points at has no permit
            // outstanding — `OwnedSemaphorePermit` holds an `Arc` for
            // exactly as long as the call it guards — so the key is idle
            // and evicting it drops no in-flight state.
            connections.retain(|_, semaphore| Arc::strong_count(semaphore) > 1);
        }
        if connections.len() >= MAX_TRACKED_KEYS {
            return self.overflow.clone();
        }

        connections
            .entry(connection_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_connection_permits)))
            .clone()
    }

    /// Take a slot in both budgets if both are free right now, or give up.
    /// The per-key slot goes first, so a flood under one key cannot occupy
    /// every platform-wide permit.
    pub fn try_acquire(
        &self,
        connection_id: &str,
    ) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let connection_semaphore = self.connection_semaphore(connection_id);
        let conn_permit = connection_semaphore.clone().try_acquire_owned().ok()?;
        let global_permit = self.platform_wide.clone().try_acquire_owned().ok()?;
        Some((conn_permit, global_permit))
    }

    /// Wait for a slot in both budgets. Acquire the connection slot first:
    /// otherwise a flood from one connection can occupy every platform-wide
    /// permit while most of its work still waits on that connection's cap.
    async fn acquire(&self, connection_id: &str) -> (OwnedSemaphorePermit, OwnedSemaphorePermit) {
        let connection_permit = self
            .connection_semaphore(connection_id)
            .acquire_owned()
            .await
            .expect("advertising per-connection quota semaphore is never closed");
        let platform_permit = self
            .platform_wide
            .clone()
            .acquire_owned()
            .await
            .expect("advertising platform-wide quota semaphore is never closed");
        (platform_permit, connection_permit)
    }

    pub fn platform_wide_permits(&self) -> usize {
        self.platform_wide.available_permits()
    }

    pub fn per_connection_permits(&self) -> usize {
        self.per_connection_permits
    }
}

/// Transparent [`AdPlatformPort`] decorator: every call queues on the
/// wrapped adapter's [`QuotaLimiter`] before delegating. Registered in place
/// of the raw adapter, never instead of it.
pub struct RateLimitedAdapter {
    inner: Arc<dyn AdPlatformPort>,
    limiter: Arc<QuotaLimiter>,
}

impl RateLimitedAdapter {
    pub fn new(inner: Arc<dyn AdPlatformPort>, limiter: Arc<QuotaLimiter>) -> Self {
        Self { inner, limiter }
    }

    pub fn limiter(&self) -> &Arc<QuotaLimiter> {
        &self.limiter
    }
}

#[async_trait]
impl AdPlatformPort for RateLimitedAdapter {
    fn descriptor(&self) -> PlatformDescriptor {
        self.inner.descriptor()
    }

    fn required_scopes(&self) -> Vec<String> {
        self.inner.required_scopes()
    }

    async fn begin_authorization(
        &self,
        connection: &ConnectionHandle,
        request: &AuthorizationRequest,
    ) -> Result<AuthorizationStart, AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner.begin_authorization(connection, request).await
    }

    async fn list_accounts(
        &self,
        connection: &ConnectionHandle,
    ) -> Result<Vec<PlatformAccount>, AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner.list_accounts(connection).await
    }

    async fn exchange_authorization(
        &self,
        exchange: &AuthorizationExchange,
    ) -> Result<ExchangedAuthorization, AdvertisingError> {
        // No connection exists yet at exchange time; every in-flight
        // exchange for this adapter shares one bucket so a burst of OAuth
        // callbacks still respects the platform-wide quota.
        let _permits = self.limiter.acquire("oauth-exchange").await;
        self.inner.exchange_authorization(exchange).await
    }

    async fn create_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign: Campaign,
        mutation_id: Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner
            .create_campaign(connection, campaign, mutation_id)
            .await
    }

    async fn get_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Option<Campaign>, AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner.get_campaign(connection, campaign_id).await
    }

    async fn update_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign: Campaign,
        mutation_id: Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner
            .update_campaign(connection, campaign, mutation_id)
            .await
    }

    async fn pause_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner.pause_campaign(connection, campaign_id).await
    }

    async fn resume_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner.resume_campaign(connection, campaign_id).await
    }

    async fn delete_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<(), AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner.delete_campaign(connection, campaign_id).await
    }

    async fn fetch_metrics(
        &self,
        connection: &ConnectionHandle,
        request: &MetricQuery,
    ) -> Result<Vec<MetricPoint>, AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner.fetch_metrics(connection, request).await
    }

    async fn upload(
        &self,
        connection: &ConnectionHandle,
        conversion: UploadedConversion,
        ids: HashedIds,
        consent: ConsentSnapshot,
    ) -> Result<(), AdvertisingError> {
        let _permits = self.limiter.acquire(&connection.id).await;
        self.inner
            .upload(connection, conversion, ids, consent)
            .await
    }
}

/// Wrap every adapter in `registry` with its own [`QuotaLimiter`] — one
/// budget per platform, matching one developer token per platform — and
/// return a fresh registry of the wrapped adapters.
pub fn rate_limited_registry<F>(
    registry: &crate::application::PlatformRegistry,
    limits_for: F,
) -> crate::application::PlatformRegistry
where
    F: Fn(&crate::domain::PlatformKey) -> (usize, usize),
{
    let mut limited = crate::application::PlatformRegistry::new();
    for adapter in registry.adapters() {
        let key = &adapter.descriptor().key;
        let (platform_wide_permits, per_connection_permits) = limits_for(key);
        let limiter = Arc::new(QuotaLimiter::new(
            platform_wide_permits,
            per_connection_permits,
        ));
        limited
            .register(Arc::new(RateLimitedAdapter::new(adapter, limiter)))
            .expect("wrapping a distinctly-keyed adapter cannot collide");
    }
    limited
}

#[cfg(test)]
mod tests {
    fn handle_tenant() -> sanvi_kernel::TenantId {
        sanvi_kernel::TenantId::try_from(1_i64).unwrap()
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::adapters::{FakeAdPlatform, contract::assert_ad_platform_contract};

    /// Tracks the maximum number of calls it ever saw in flight
    /// simultaneously, per connection id.
    struct ConcurrencyProbe {
        in_flight: AtomicUsize,
        max_observed: AtomicUsize,
        descriptor: PlatformDescriptor,
    }

    #[async_trait]
    impl AdPlatformPort for ConcurrencyProbe {
        fn descriptor(&self) -> PlatformDescriptor {
            self.descriptor.clone()
        }

        fn required_scopes(&self) -> Vec<String> {
            vec!["ads.read".to_string(), "ads.manage".to_string()]
        }

        async fn begin_authorization(
            &self,
            _connection: &ConnectionHandle,
            _request: &AuthorizationRequest,
        ) -> Result<AuthorizationStart, AdvertisingError> {
            unimplemented!()
        }

        async fn list_accounts(
            &self,
            _connection: &ConnectionHandle,
        ) -> Result<Vec<PlatformAccount>, AdvertisingError> {
            unimplemented!()
        }

        async fn exchange_authorization(
            &self,
            _exchange: &AuthorizationExchange,
        ) -> Result<ExchangedAuthorization, AdvertisingError> {
            unimplemented!()
        }

        async fn create_campaign(
            &self,
            _connection: &ConnectionHandle,
            campaign: Campaign,
            _mutation_id: Uuid,
        ) -> Result<Campaign, AdvertisingError> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_observed.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(campaign)
        }

        async fn get_campaign(
            &self,
            _connection: &ConnectionHandle,
            _campaign_id: &str,
        ) -> Result<Option<Campaign>, AdvertisingError> {
            unimplemented!()
        }

        async fn update_campaign(
            &self,
            _connection: &ConnectionHandle,
            campaign: Campaign,
            _mutation_id: Uuid,
        ) -> Result<Campaign, AdvertisingError> {
            Ok(campaign)
        }

        async fn pause_campaign(
            &self,
            _connection: &ConnectionHandle,
            _campaign_id: &str,
        ) -> Result<Campaign, AdvertisingError> {
            unimplemented!()
        }

        async fn resume_campaign(
            &self,
            _connection: &ConnectionHandle,
            _campaign_id: &str,
        ) -> Result<Campaign, AdvertisingError> {
            unimplemented!()
        }

        async fn delete_campaign(
            &self,
            _connection: &ConnectionHandle,
            _campaign_id: &str,
        ) -> Result<(), AdvertisingError> {
            unimplemented!()
        }

        async fn fetch_metrics(
            &self,
            _connection: &ConnectionHandle,
            _request: &MetricQuery,
        ) -> Result<Vec<MetricPoint>, AdvertisingError> {
            unimplemented!()
        }

        async fn upload(
            &self,
            _connection: &ConnectionHandle,
            _conversion: UploadedConversion,
            _ids: HashedIds,
            _consent: ConsentSnapshot,
        ) -> Result<(), AdvertisingError> {
            unimplemented!()
        }
    }

    fn draft_campaign(id: &str) -> Campaign {
        Campaign {
            id: id.to_string(),
            name: "probe".to_string(),
            status: crate::domain::CampaignStatus::Draft,
            objective: crate::domain::Objective::parse("sales").unwrap(),
            budget: crate::domain::Budget {
                kind: crate::domain::BudgetType::Daily,
                amount: crate::domain::Money::new(100, "JPY").unwrap(),
            },
            schedule: None,
            ad_groups: Vec::new(),
            drift: crate::domain::DriftState {
                drifted: false,
                changed_fields: Vec::new(),
            },
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn per_connection_cap_bounds_concurrency_on_one_connection() {
        let probe = Arc::new(ConcurrencyProbe {
            in_flight: AtomicUsize::new(0),
            max_observed: AtomicUsize::new(0),
            descriptor: FakeAdPlatform::fixtures().remove(0),
        });
        let limiter = Arc::new(QuotaLimiter::new(100, 3));
        let limited = Arc::new(RateLimitedAdapter::new(probe.clone(), limiter));

        let mut handles = Vec::new();
        for i in 0..12 {
            let limited = limited.clone();
            handles.push(tokio::spawn(async move {
                let connection = ConnectionHandle {
                    id: "connection-a".to_string(),
                    tenant_id: handle_tenant(),
                    external_account_id: "connection-a".to_string(),
                };
                limited
                    .create_campaign(
                        &connection,
                        draft_campaign(&format!("c{i}")),
                        Uuid::now_v7(),
                    )
                    .await
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        assert!(
            probe.max_observed.load(Ordering::SeqCst) <= 3,
            "per-connection cap of 3 was exceeded: saw {}",
            probe.max_observed.load(Ordering::SeqCst)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn a_busy_connection_does_not_starve_another_connections_platform_wide_budget() {
        let probe = Arc::new(ConcurrencyProbe {
            in_flight: AtomicUsize::new(0),
            max_observed: AtomicUsize::new(0),
            descriptor: FakeAdPlatform::fixtures().remove(0),
        });
        // Platform-wide budget is generous; the busy connection's own cap
        // (2) is what must stop it from consuming every platform-wide slot.
        let limiter = Arc::new(QuotaLimiter::new(10, 2));
        let limited = Arc::new(RateLimitedAdapter::new(probe.clone(), limiter));

        // Flood connection A with far more work than its own cap allows.
        let mut handles = Vec::new();
        for i in 0..20 {
            let limited = limited.clone();
            handles.push(tokio::spawn(async move {
                let connection = ConnectionHandle {
                    id: "connection-a".to_string(),
                    tenant_id: handle_tenant(),
                    external_account_id: "connection-a".to_string(),
                };
                limited
                    .create_campaign(
                        &connection,
                        draft_campaign(&format!("a{i}")),
                        Uuid::now_v7(),
                    )
                    .await
                    .unwrap();
            }));
        }

        // Connection B should still get served promptly rather than queue
        // behind A's flood.
        let start = tokio::time::Instant::now();
        let connection_b = ConnectionHandle {
            id: "connection-b".to_string(),
            tenant_id: handle_tenant(),
            external_account_id: "connection-b".to_string(),
        };
        limited
            .create_campaign(&connection_b, draft_campaign("b0"), Uuid::now_v7())
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "connection B waited {:?} behind connection A's flood",
            start.elapsed()
        );

        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rate_limited_adapter_still_satisfies_the_shared_port_contract() {
        let registry = FakeAdPlatform::registered_catalog();
        let limiter = Arc::new(QuotaLimiter::new(10, 5));
        let key = crate::domain::PlatformKey::parse("google_ads").unwrap();
        let inner = registry.adapter(&key).unwrap();
        let limited = RateLimitedAdapter::new(inner, limiter);
        assert_ad_platform_contract(&limited).await.unwrap();
    }

    /// The public beacon keys this limiter by attacker-chosen strings, so
    /// the table must not grow with the number of distinct values seen.
    #[test]
    fn idle_keys_are_evicted_rather_than_retained_forever() {
        let limiter = QuotaLimiter::new(100, 2);
        for i in 0..(MAX_TRACKED_KEYS * 2) {
            // Each permit is dropped at the end of the iteration, leaving
            // the key idle and therefore evictable.
            let _permits = limiter.try_acquire(&format!("origin-{i}"));
        }
        let tracked = limiter.connections.lock().unwrap().len();
        assert!(
            tracked <= MAX_TRACKED_KEYS,
            "limiter tracked {tracked} keys, above the {MAX_TRACKED_KEYS} ceiling"
        );
    }

    /// Eviction must never hand out a fresh budget to a key that still has
    /// calls in flight — that would let one key exceed its own cap.
    #[test]
    fn a_busy_key_survives_eviction_pressure() {
        let limiter = QuotaLimiter::new(100, 2);
        let held = limiter.try_acquire("busy").expect("first permit is free");
        for i in 0..(MAX_TRACKED_KEYS * 2) {
            let _permits = limiter.try_acquire(&format!("origin-{i}"));
        }
        // Cap of 2: one permit is still held, so exactly one more is free.
        let second = limiter.try_acquire("busy");
        assert!(second.is_some(), "busy key lost its second permit");
        assert!(
            limiter.try_acquire("busy").is_none(),
            "busy key was re-keyed to a fresh budget and exceeded its cap"
        );
        drop((held, second));
    }

    #[test]
    fn rate_limited_registry_preserves_every_platform_key() {
        let registry = FakeAdPlatform::registered_catalog();
        let expected: std::collections::BTreeSet<_> = registry
            .adapters()
            .iter()
            .map(|adapter| adapter.descriptor().key)
            .collect();
        let limited = rate_limited_registry(&registry, |_| (10, 5));
        let actual: std::collections::BTreeSet<_> = limited
            .adapters()
            .iter()
            .map(|adapter| adapter.descriptor().key)
            .collect();
        assert_eq!(expected, actual);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn rate_limited_registry_assigns_distinct_budgets_per_platform() {
        let google_key = crate::domain::PlatformKey::parse("google_ads").unwrap();
        let meta_key = crate::domain::PlatformKey::parse("meta").unwrap();

        let google_probe = Arc::new(ConcurrencyProbe {
            in_flight: AtomicUsize::new(0),
            max_observed: AtomicUsize::new(0),
            descriptor: FakeAdPlatform::fixtures().remove(0),
        });
        let meta_probe = Arc::new(ConcurrencyProbe {
            in_flight: AtomicUsize::new(0),
            max_observed: AtomicUsize::new(0),
            descriptor: FakeAdPlatform::fixtures().remove(1),
        });

        let mut raw_registry = crate::application::PlatformRegistry::new();
        raw_registry.register(google_probe.clone()).unwrap();
        raw_registry.register(meta_probe.clone()).unwrap();

        let limited = rate_limited_registry(&raw_registry, |platform| {
            if platform == &google_key {
                (20, 5)
            } else if platform == &meta_key {
                (10, 2)
            } else {
                (1, 1)
            }
        });

        let google_adapter = limited.adapter(&google_key).unwrap();
        let meta_adapter = limited.adapter(&meta_key).unwrap();

        // Exercise Meta with 10 concurrent requests on connection-1 -> capped at 2
        let mut handles = Vec::new();
        for i in 0..10 {
            let meta_adapter = meta_adapter.clone();
            handles.push(tokio::spawn(async move {
                let connection = ConnectionHandle {
                    id: "connection-meta-1".to_string(),
                    tenant_id: handle_tenant(),
                    external_account_id: "connection-meta-1".to_string(),
                };
                meta_adapter
                    .create_campaign(
                        &connection,
                        draft_campaign(&format!("m{i}")),
                        Uuid::now_v7(),
                    )
                    .await
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(meta_probe.max_observed.load(Ordering::SeqCst), 2);

        // Exercise Google with 10 concurrent requests on connection-1 -> capped at 5
        let mut handles = Vec::new();
        for i in 0..10 {
            let google_adapter = google_adapter.clone();
            handles.push(tokio::spawn(async move {
                let connection = ConnectionHandle {
                    id: "connection-google-1".to_string(),
                    tenant_id: handle_tenant(),
                    external_account_id: "connection-google-1".to_string(),
                };
                google_adapter
                    .create_campaign(
                        &connection,
                        draft_campaign(&format!("g{i}")),
                        Uuid::now_v7(),
                    )
                    .await
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(google_probe.max_observed.load(Ordering::SeqCst), 5);
    }
}
