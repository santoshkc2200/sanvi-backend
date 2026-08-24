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
    AdPlatformPort, AdvertisingError, AuthorizationRequest, AuthorizationStart, Campaign,
    ConnectionHandle, ConsentSnapshot, HashedIds, MetricPoint, MetricQuery, PlatformAccount,
    PlatformDescriptor, UploadedConversion,
};

/// Bounds concurrent adapter calls two ways at once: a platform-wide cap
/// (the shared developer-token quota) and a per-connection cap nested
/// inside it (so one connection cannot claim every platform-wide slot).
/// A caller always holds both permits or neither.
pub struct QuotaLimiter {
    platform_wide: Arc<Semaphore>,
    per_connection_permits: usize,
    connections: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl QuotaLimiter {
    pub fn new(platform_wide_permits: usize, per_connection_permits: usize) -> Self {
        Self {
            platform_wide: Arc::new(Semaphore::new(platform_wide_permits.max(1))),
            per_connection_permits: per_connection_permits.max(1),
            connections: Mutex::new(HashMap::new()),
        }
    }

    fn connection_semaphore(&self, connection_id: &str) -> Arc<Semaphore> {
        let mut connections = self
            .connections
            .lock()
            .expect("advertising quota limiter mutex poisoned");
        connections
            .entry(connection_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_connection_permits)))
            .clone()
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
}

#[async_trait]
impl AdPlatformPort for RateLimitedAdapter {
    fn descriptor(&self) -> PlatformDescriptor {
        self.inner.descriptor()
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
pub fn rate_limited_registry(
    registry: &crate::application::PlatformRegistry,
    platform_wide_permits: usize,
    per_connection_permits: usize,
) -> crate::application::PlatformRegistry {
    let mut limited = crate::application::PlatformRegistry::new();
    for adapter in registry.adapters() {
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

    #[test]
    fn rate_limited_registry_preserves_every_platform_key() {
        let registry = FakeAdPlatform::registered_catalog();
        let expected: std::collections::BTreeSet<_> = registry
            .adapters()
            .iter()
            .map(|adapter| adapter.descriptor().key)
            .collect();
        let limited = rate_limited_registry(&registry, 10, 5);
        let actual: std::collections::BTreeSet<_> = limited
            .adapters()
            .iter()
            .map(|adapter| adapter.descriptor().key)
            .collect();
        assert_eq!(expected, actual);
    }
}
