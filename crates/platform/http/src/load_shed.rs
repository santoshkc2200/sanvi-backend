//! Load shedding with per-tenant fairness (TASK-023, FR-1114; the
//! `platform.load_shedding` flag).
//!
//! Admission control above the concurrency ceiling TASK-021 measured.
//! Above the ceiling — or while the pool's probe acquire wait exceeds its
//! threshold, or while a tenant holds more than its share under contention
//! — requests get `503 platform/overloaded` with `Retry-After` instead of
//! queueing unboundedly. Webhooks and payment callbacks are never shed
//! (NFR-1106), whatever the ceiling says.
//!
//! The state is process-global: the composition root builds one
//! [`LoadShedState`] and threads it through `middleware::stack` like the
//! degraded registry, so every worker sheds against the same counters.
//! `None` (tests that never enable shedding) is a pass-through.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use actix_web::{
    HttpMessage, ResponseError,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse, Transform},
};

use crate::config::LoadSheddingConfig;

/// Why a request was shed — for the log line, not the wire: every shed is
/// the same `503 platform/overloaded` with `Retry-After`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedReason {
    /// Above the global in-flight ceiling.
    GlobalCeiling,
    /// A hot tenant past its share while the platform is contended.
    TenantShare,
    /// The pool probe wait exceeds its threshold.
    PoolPressure,
}

/// Process-global shedding counters. Built once by the composition root.
pub struct LoadShedState {
    config: LoadSheddingConfig,
    in_flight: AtomicUsize,
    per_tenant: Mutex<HashMap<String, usize>>,
    pressure: Option<Arc<sanvi_resilience::PoolPressure>>,
}

/// What [`LoadShedState::admit`] decided.
pub enum AdmitOutcome {
    /// Admitted; the guard releases the slots on drop.
    Admitted(InFlightGuard),
    /// Shed with the carried reason.
    Shed(ShedReason),
}

/// Holds one global slot and one tenant slot until dropped. Guards from
/// the pass-through paths (flag off, exempt prefix) hold nothing.
pub struct InFlightGuard {
    state: Arc<LoadShedState>,
    tenant: Option<String>,
    accounted: bool,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if !self.accounted {
            return;
        }
        self.state.in_flight.fetch_sub(1, Ordering::SeqCst);
        if let Some(tenant) = &self.tenant {
            let mut per_tenant = self.state.per_tenant.lock().expect("shed lock");
            if let Some(count) = per_tenant.get_mut(tenant) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    per_tenant.remove(tenant);
                }
            }
        }
    }
}

impl LoadShedState {
    pub fn new(
        config: LoadSheddingConfig,
        pressure: Option<Arc<sanvi_resilience::PoolPressure>>,
    ) -> Self {
        Self {
            config,
            in_flight: AtomicUsize::new(0),
            per_tenant: Mutex::new(HashMap::new()),
            pressure,
        }
    }

    /// Test/current-depth introspection: requests currently admitted.
    #[cfg(test)]
    fn depth(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Decide admission for one request. Tenant slots are only counted
    /// under contention (at or above `fairness_knee` globally) — below the
    /// knee every tenant may use the whole ceiling.
    pub fn admit(self: &Arc<Self>, path: &str, tenant: Option<&str>) -> AdmitOutcome {
        if !self.config.enabled {
            return AdmitOutcome::Admitted(InFlightGuard {
                state: self.clone(),
                tenant: None,
                accounted: false,
            });
        }
        if self
            .config
            .exempt_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            return AdmitOutcome::Admitted(InFlightGuard {
                state: self.clone(),
                tenant: None,
                accounted: false,
            });
        }
        // The pool is the bottleneck the spike run recorded: shed on its
        // signal before the 1 s acquire waits, without consuming a slot.
        if let Some(pressure) = &self.pressure
            && let Some(wait) = pressure.current_wait()
            && wait.as_micros() >= u128::from(self.config.pool_wait_micros)
        {
            return AdmitOutcome::Shed(ShedReason::PoolPressure);
        }
        let global = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        if global > self.config.in_flight_ceiling {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return AdmitOutcome::Shed(ShedReason::GlobalCeiling);
        }
        let tenant_key = if global >= self.config.fairness_knee {
            tenant.map(str::to_string)
        } else {
            None
        };
        if let Some(key) = &tenant_key {
            let mut per_tenant = self.per_tenant.lock().expect("shed lock");
            let count = per_tenant.entry(key.clone()).or_insert(0);
            if *count >= self.config.per_tenant_in_flight {
                drop(per_tenant);
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                return AdmitOutcome::Shed(ShedReason::TenantShare);
            }
            *count += 1;
        }
        AdmitOutcome::Admitted(InFlightGuard {
            state: self.clone(),
            tenant: tenant_key,
            accounted: true,
        })
    }
}

/// The flag-gated shedding layer. `None` is a pass-through; `Some` admits
/// through the shared [`LoadShedState`].
#[derive(Clone)]
pub struct LoadShedding {
    state: Option<Arc<LoadShedState>>,
}

impl LoadShedding {
    pub fn new(state: Option<Arc<LoadShedState>>) -> Self {
        Self { state }
    }
}

impl<S> Transform<S, ServiceRequest> for LoadShedding
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = LoadSheddingService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(LoadSheddingService {
            service,
            state: self.state.clone(),
        }))
    }
}

pub struct LoadSheddingService<S> {
    service: S,
    state: Option<Arc<LoadShedState>>,
}

impl<S> actix_web::dev::Service<ServiceRequest> for LoadSheddingService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let state = self.state.clone();
        let Some(state) = state else {
            return Box::pin(self.service.call(req));
        };
        let path = req.path().to_string();
        let tenant = req
            .extensions()
            .get::<crate::tenancy::TenantContext>()
            .map(|context| context.tenant_id.to_string());
        match state.admit(&path, tenant.as_deref()) {
            AdmitOutcome::Admitted(guard) => {
                let fut = self.service.call(req);
                Box::pin(async move {
                    let result = fut.await;
                    drop(guard);
                    result
                })
            }
            AdmitOutcome::Shed(reason) => {
                let retry_after = state.config.retry_after_secs;
                tracing::warn!(
                    path = %path,
                    tenant = %tenant.as_deref().unwrap_or("unresolved"),
                    reason = ?reason,
                    "request shed: platform overloaded"
                );
                let response = crate::error::ApiError::Overloaded {
                    retry_after_secs: retry_after,
                }
                .error_response()
                .map_into_boxed_body();
                Box::pin(async move { Ok(req.into_response(response)) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> LoadSheddingConfig {
        LoadSheddingConfig {
            enabled: true,
            in_flight_ceiling: 2,
            per_tenant_in_flight: 1,
            fairness_knee: 2,
            pool_wait_micros: 250_000,
            retry_after_secs: 5,
            exempt_prefixes: vec![
                "/api/v1/webhooks".to_string(),
                "/api/v1/billing/webhooks".to_string(),
                "/api/v1/payments/webhooks".to_string(),
                "/healthz".to_string(),
            ],
        }
    }

    fn state() -> Arc<LoadShedState> {
        Arc::new(LoadShedState::new(config(), None))
    }

    fn admitted(outcome: AdmitOutcome) -> Option<InFlightGuard> {
        match outcome {
            AdmitOutcome::Admitted(guard) => Some(guard),
            AdmitOutcome::Shed(reason) => panic!("expected admission, shed: {reason:?}"),
        }
    }

    fn shed_reason(outcome: AdmitOutcome) -> ShedReason {
        match outcome {
            AdmitOutcome::Shed(reason) => reason,
            AdmitOutcome::Admitted(_) => panic!("expected shed, admitted"),
        }
    }

    #[test]
    fn disabled_config_admits_everything() {
        let off = Arc::new(LoadShedState::new(
            LoadSheddingConfig {
                enabled: false,
                ..config()
            },
            None,
        ));
        let mut guards = Vec::new();
        for _ in 0..10 {
            guards.push(admitted(off.admit("/api/v1/tenant/x", Some("t1"))));
        }
        assert_eq!(off.depth(), 0, "flag off counts nothing");
    }

    #[test]
    fn above_the_ceiling_sheds_and_recovery_admits() {
        let state = state();
        let _first = admitted(state.admit("/api/v1/tenant/a", None));
        let _second = admitted(state.admit("/api/v1/tenant/b", None));
        assert_eq!(
            shed_reason(state.admit("/api/v1/tenant/c", None)),
            ShedReason::GlobalCeiling
        );
        assert_eq!(state.depth(), 2, "a shed request holds no slot");
        drop(_second);
        let _third = admitted(state.admit("/api/v1/tenant/c", None));
    }

    #[test]
    fn exempt_paths_are_never_shed() {
        let state = state();
        let _first = admitted(state.admit("/api/v1/tenant/a", None));
        let _second = admitted(state.admit("/api/v1/tenant/b", None));
        // At the ceiling — a webhook and a payment callback still admit.
        let _hook = admitted(state.admit("/api/v1/webhooks/stripe", None));
        let _billing_hook = admitted(state.admit("/api/v1/billing/webhooks", None));
        let _health = admitted(state.admit("/healthz", None));
        // …while an ordinary request sheds.
        assert_eq!(
            shed_reason(state.admit("/api/v1/tenant/c", None)),
            ShedReason::GlobalCeiling
        );
    }

    #[test]
    fn below_the_knee_one_tenant_may_use_the_whole_ceiling() {
        let state = Arc::new(LoadShedState::new(
            LoadSheddingConfig {
                fairness_knee: 100,
                ..config()
            },
            None,
        ));
        let _first = admitted(state.admit("/t", Some("hot")));
        let _second = admitted(state.admit("/t", Some("hot")));
    }

    #[test]
    fn under_contention_a_hot_tenant_sheds_ahead_of_its_neighbours() {
        // Ceiling 4, knee 2, per-tenant share 1: tenant "hot" holds two
        // slots (one below the knee, one at it), then its third request
        // sheds while a cold tenant still admits.
        let state = Arc::new(LoadShedState::new(
            LoadSheddingConfig {
                in_flight_ceiling: 4,
                per_tenant_in_flight: 1,
                fairness_knee: 2,
                ..config()
            },
            None,
        ));
        let _hot_first = admitted(state.admit("/t", Some("hot")));
        let _hot_second = admitted(state.admit("/t", Some("hot")));
        assert_eq!(
            shed_reason(state.admit("/t", Some("hot"))),
            ShedReason::TenantShare
        );
        let _cold = admitted(state.admit("/t", Some("cold")));
        assert_eq!(state.depth(), 3);
    }

    #[test]
    fn pool_pressure_sheds_without_consuming_a_slot() {
        let pressure = Arc::new(sanvi_resilience::PoolPressure::new());
        let state = Arc::new(LoadShedState::new(config(), Some(pressure.clone())));
        pressure.sample(std::time::Duration::from_micros(300_000));
        assert_eq!(
            shed_reason(state.admit("/api/v1/tenant/a", None)),
            ShedReason::PoolPressure
        );
        assert_eq!(state.depth(), 0);
        // A fresh pool admits again.
        pressure.sample(std::time::Duration::from_micros(1_000));
        let _ok = admitted(state.admit("/api/v1/tenant/a", None));
    }

    #[test]
    fn a_stale_pressure_sample_sheds_nothing() {
        let pressure = Arc::new(sanvi_resilience::PoolPressure::new());
        let state = Arc::new(LoadShedState::new(config(), Some(pressure.clone())));
        // No sample at all: the pool signal is simply absent.
        let _ok = admitted(state.admit("/api/v1/tenant/a", None));
        drop(_ok);
        assert_eq!(state.depth(), 0);
    }

    /// Through the full middleware stack: with shedding enabled at a
    /// ceiling of one, a held slot sheds the next request as `503
    /// platform/overloaded` with `Retry-After` — without it ever reaching
    /// the handler — and releasing the slot admits again.
    #[actix_web::test]
    async fn the_stack_sheds_above_the_configured_ceiling() {
        use actix_web::{App, HttpResponse, test, web};

        let shed_config = crate::config::LoadSheddingConfig {
            enabled: true,
            in_flight_ceiling: 1,
            per_tenant_in_flight: 1,
            fairness_knee: 1,
            pool_wait_micros: u64::MAX,
            retry_after_secs: 2,
            exempt_prefixes: vec!["/healthz".to_string()],
        };
        let state = Arc::new(LoadShedState::new(shed_config, None));

        // The layer admits through the passed state only; the server
        // config's flag is the composition root's concern.
        let server_config = crate::config::ServerConfig::default();
        let app = test::init_service(crate::middleware::stack(
            App::new().route(
                "/probe",
                web::get().to(|| async { HttpResponse::Ok().finish() }),
            ),
            server_config,
            None,
            None,
            None,
            None,
            None,
            crate::ssr_cache::SsrCacheLayer::disabled(crate::config::SsrCacheConfig::default()),
            crate::degraded::DegradedHeaderLayer::disabled(),
            Some(state.clone()),
        ))
        .await;

        // Hold the only slot outside the stack: the next request must shed
        // without ever reaching the handler.
        let held = admitted(state.admit("/probe", None));
        let shed =
            test::call_service(&app, test::TestRequest::get().uri("/probe").to_request()).await;
        assert_eq!(shed.status().as_u16(), 503);
        assert_eq!(
            shed.headers()
                .get("retry-after")
                .map(|v| v.to_str().unwrap().to_string())
                .as_deref(),
            Some("2"),
            "a shed request tells the client when to retry"
        );
        let body: serde_json::Value = test::read_body_json(shed).await;
        assert_eq!(
            body["type"], "https://sanvi.dev/problems/platform/overloaded",
            "shedding is the platform/overloaded problem, not a generic 503"
        );

        // Exempt paths admit even at the ceiling.
        let health =
            test::call_service(&app, test::TestRequest::get().uri("/healthz").to_request()).await;
        assert_eq!(
            health.status().as_u16(),
            404,
            "exempt paths pass the shedder (no such route here)"
        );

        // Release the slot: admission recovers.
        drop(held);
        let ok =
            test::call_service(&app, test::TestRequest::get().uri("/probe").to_request()).await;
        assert_eq!(ok.status().as_u16(), 200, "the admitted request completes");
    }
}
