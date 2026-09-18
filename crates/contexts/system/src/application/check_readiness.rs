use std::{sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::{DependencyStatus, HealthProbe, HealthState, Readiness};

/// Query: are all dependencies healthy? Degrades per-dependency.
pub struct CheckReadiness {
    probes: Vec<(String, Arc<dyn HealthProbe>)>,
    budget: Duration,
}

impl CheckReadiness {
    pub fn new(probes: Vec<Arc<dyn HealthProbe>>, budget: Duration) -> Self {
        let probes = probes
            .into_iter()
            .map(|probe| (probe.name().to_string(), probe))
            .collect();
        Self { probes, budget }
    }

    /// Probe every dependency concurrently under a shared time budget.
    #[tracing::instrument(name = "system.check_readiness", skip_all)]
    pub async fn execute(&self) -> ReadinessOutput {
        let checks = futures_concurrent(self.probes.clone(), self.budget).await;
        let all_ok = checks.iter().all(|check| check.state == HealthState::Ok);
        ReadinessOutput {
            readiness: Readiness {
                status: if all_ok {
                    HealthState::Ok
                } else {
                    HealthState::Degraded
                },
                checks,
            },
        }
    }
}

async fn futures_concurrent(
    probes: Vec<(String, Arc<dyn HealthProbe>)>,
    budget: Duration,
) -> Vec<DependencyStatus> {
    let tasks: Vec<_> = probes
        .into_iter()
        .map(|(name, probe)| {
            tokio::spawn(async move {
                match probe.check(budget).await {
                    Ok(()) => DependencyStatus::healthy(name),
                    Err(err) => DependencyStatus::degraded(name, err.to_string()),
                }
            })
        })
        .collect();

    let mut checks = Vec::with_capacity(tasks.len());
    for task in tasks {
        checks.push(
            task.await
                .unwrap_or_else(|_| DependencyStatus::degraded("unknown", "probe task panicked")),
        );
    }
    checks
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReadinessOutput {
    #[serde(flatten)]
    pub readiness: Readiness,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::domain::SystemError;

    use super::*;

    struct OkProbe(&'static str);
    #[allow(dead_code)]
    struct SlowProbe(&'static str);
    struct FailProbe(&'static str);

    #[async_trait::async_trait]
    impl HealthProbe for OkProbe {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn check(&self, _budget: Duration) -> Result<(), SystemError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl HealthProbe for SlowProbe {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn check(&self, budget: Duration) -> Result<(), SystemError> {
            tokio::time::sleep(budget * 2).await;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl HealthProbe for FailProbe {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn check(&self, _budget: Duration) -> Result<(), SystemError> {
            Err(SystemError::DependencyUnavailable(
                self.0.to_string(),
                "connection refused".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn all_healthy_means_ready() {
        let use_case = CheckReadiness::new(
            vec![Arc::new(OkProbe("database")), Arc::new(OkProbe("redis"))],
            Duration::from_millis(500),
        );
        let output = use_case.execute().await;
        assert!(output.readiness.is_ready());
        assert_eq!(output.readiness.checks.len(), 2);
    }

    #[tokio::test]
    async fn one_failing_dependency_degrades_the_whole() {
        let use_case = CheckReadiness::new(
            vec![Arc::new(OkProbe("database")), Arc::new(FailProbe("redis"))],
            Duration::from_millis(500),
        );
        let output = use_case.execute().await;
        assert!(!output.readiness.is_ready());
        assert_eq!(output.readiness.status, HealthState::Degraded);
        let redis = output
            .readiness
            .checks
            .iter()
            .find(|check| check.name == "redis")
            .unwrap();
        assert_eq!(redis.state, HealthState::Degraded);
        assert!(
            redis
                .detail
                .as_deref()
                .unwrap()
                .contains("connection refused")
        );
    }

    #[tokio::test]
    async fn deadline_is_shared_across_probes_and_they_run_concurrently() {
        static ACTIVE: AtomicUsize = AtomicUsize::new(0);

        struct TrackingProbe;
        #[async_trait::async_trait]
        impl HealthProbe for TrackingProbe {
            fn name(&self) -> &'static str {
                "slow"
            }
            async fn check(&self, _budget: Duration) -> Result<(), SystemError> {
                let _ = ACTIVE.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                let _ = ACTIVE.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let use_case = CheckReadiness::new(
            vec![
                Arc::new(TrackingProbe),
                Arc::new(TrackingProbe),
                Arc::new(TrackingProbe),
            ],
            Duration::from_millis(500),
        );
        let started = std::time::Instant::now();
        let output = use_case.execute().await;
        // Three sequential 30ms probes would take ~90ms; concurrent, ~30ms.
        assert!(started.elapsed() < Duration::from_millis(80));
        assert!(output.readiness.is_ready());
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
    }
}
