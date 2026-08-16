//! `sanvi-worker` — background worker: outbox dispatcher, stream consumers
//! and the leader-elected scheduler.
//!
//! - Outbox dispatcher relays unpublished domain events to per-context
//!   Redis Streams, dead-lettering rows that exhaust retries.
//! - Consumers process `sanvi.events.tenancy` (cache invalidation) with
//!   exactly-once semantics via `platform.processed_events`.
//! - The scheduler runs periodic jobs (purge sweep, idempotency sweep) on
//!   the elected leader only.
//! - Graceful shutdown: SIGTERM/SIGINT stops polling, in-flight batches
//!   complete, pools close.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use sanvi_access::adapters::{HttpKetoClient, SqlxRoleRepository};
use sanvi_access::domain::{PermissionStore, RoleRepository};
use sanvi_messaging::{
    ConsumerMetrics, FnJob, OutboxDispatcher, OutboxMetrics, RedisConfig, Scheduler,
    StreamConsumer, StreamName, StreamsClient, connect_redis,
};
use sanvi_persistence::{DatabaseConfig, build_pool};
use sanvi_telemetry::TelemetryConfig;
use sanvi_tenancy::{
    TenancyConfig,
    adapters::{
        OutboxEventPublisher, SystemClock, events::tenant_event_handler,
        repo::SqlxTenantRepository, resolution::RedisTenantResolutionCache,
    },
    application::{PurgeSweep, ResolveTenant},
};
use serde::{Deserialize, Serialize};

/// Outbox dispatcher tuning.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OutboxConfig {
    pub poll_interval_ms: u64,
    pub batch_size: usize,
    pub max_attempts: i32,
    pub stream: String,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            batch_size: 100,
            max_attempts: 5,
            stream: "sanvi.events".to_string(),
        }
    }
}

/// Scheduler tuning.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SchedulerConfig {
    /// Interval of the purge sweep (archived tenants → purge requests).
    pub purge_sweep_interval_secs: u64,
    /// Retention before an archived tenant gets a purge request (days).
    pub purge_retention_days: u32,
    /// Idempotency-key sweep interval.
    pub idempotency_sweep_interval_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            purge_sweep_interval_secs: 3600,
            purge_retention_days: 30,
            idempotency_sweep_interval_secs: 3600,
        }
    }
}

/// Keto endpoints the access sync consumer needs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OryConfig {
    pub keto_read_url: String,
    pub keto_write_url: String,
}

impl Default for OryConfig {
    fn default() -> Self {
        Self {
            keto_read_url: "http://localhost:4466".to_string(),
            keto_write_url: "http://localhost:4467".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerConfig {
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub telemetry: TelemetryConfig,
    pub outbox: OutboxConfig,
    pub scheduler: SchedulerConfig,
    pub tenancy: TenancyConfig,
    #[serde(default)]
    pub ory: OryConfig,
}

impl WorkerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.database.validate()?;
        self.redis.validate()?;
        self.telemetry.validate()?;
        self.tenancy.validate()?;
        if self.outbox.batch_size == 0 {
            anyhow::bail!("outbox.batch_size must be >= 1");
        }
        if self.scheduler.purge_retention_days == 0 {
            anyhow::bail!("scheduler.purge_retention_days must be >= 1");
        }
        Ok(())
    }
}

fn load_config() -> anyhow::Result<WorkerConfig> {
    let environment = std::env::var("SANVI_ENV").unwrap_or_else(|_| "local".to_string());
    let config: WorkerConfig = Figment::new()
        .merge(Toml::file("configs/base.toml"))
        .merge(Toml::file(format!("configs/{environment}.toml")))
        .merge(Env::prefixed("SANVI__").split("__"))
        .extract()
        .context("failed to load configuration")?;
    config.validate()?;
    Ok(config)
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = match load_config() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("configuration error:\n{err:#}");
            return ExitCode::from(2);
        }
    };

    let (metrics, mut telemetry_guard) = match sanvi_telemetry::init(config.telemetry.clone()) {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("failed to initialise telemetry: {err:#}");
            return ExitCode::from(2);
        }
    };

    let pool = match build_pool(&config.database).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("failed to connect to postgres: {err:#}");
            return ExitCode::from(1);
        }
    };

    let redis = match connect_redis(&config.redis).await {
        Ok(manager) => manager,
        Err(err) => {
            eprintln!("failed to connect to redis: {err:#}");
            return ExitCode::from(1);
        }
    };

    let registry = metrics.prometheus_registry();
    let outbox_metrics = Arc::new(OutboxMetrics::new());
    if let Err(err) = outbox_metrics.register(registry) {
        eprintln!("failed to register outbox metrics: {err}");
        return ExitCode::from(1);
    }
    let consumer_metrics = Arc::new(ConsumerMetrics::new("tenancy"));
    if let Err(err) = consumer_metrics.register(registry) {
        eprintln!("failed to register consumer metrics: {err}");
        return ExitCode::from(1);
    }

    let streams = StreamsClient::new(redis.clone());

    let dispatcher = OutboxDispatcher::new(
        pool.clone(),
        streams.clone(),
        StreamName(config.outbox.stream.clone()),
        config.outbox.batch_size,
        config.outbox.max_attempts,
    )
    .with_metrics(outbox_metrics.clone());

    // --- tenancy consumer: keeps resolution caches coherent -----------------
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());
    let tenants: Arc<dyn sanvi_tenancy::domain::TenantRepository> =
        Arc::new(SqlxTenantRepository::new());
    let resolution_cache = Arc::new(RedisTenantResolutionCache::new(
        redis.clone(),
        Duration::from_secs(config.tenancy.resolution_cache_ttl_secs),
    ));
    let resolve = Arc::new(ResolveTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        resolution_cache.clone(),
    ));
    let invalidate = {
        let resolve = resolve.clone();
        move |slug: String, tenant_id: uuid::Uuid| {
            let resolve = resolve.clone();
            tokio::spawn(async move {
                resolve.invalidate(&slug, tenant_id).await;
            });
        }
    };
    let consumer =
        StreamConsumer::new(pool.clone(), streams.clone()).with_metrics(consumer_metrics.clone());
    let consumer_name = format!("tenancy-{}", uuid::Uuid::now_v7());

    // --- access consumer: keeps Keto in sync with identity events -----------
    let access_store: Arc<dyn PermissionStore> = match HttpKetoClient::new(
        config.ory.keto_read_url.clone(),
        config.ory.keto_write_url.clone(),
    ) {
        Ok(client) => Arc::new(client),
        Err(err) => {
            eprintln!("failed to build the keto client: {err}");
            return ExitCode::from(2);
        }
    };
    let access_roles: Arc<dyn RoleRepository> = Arc::new(SqlxRoleRepository::new());
    let access_tx = sanvi_persistence::TxManager::new(pool.clone());
    let role_key_resolver = {
        let roles = access_roles.clone();
        let tx = access_tx.clone();
        move |role_id: uuid::Uuid| {
            let roles = roles.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut scope = match tx.begin_unscoped("access_role_key_lookup", "access").await {
                    Ok(scope) => scope,
                    Err(err) => {
                        tracing::error!(error = %err, "role key lookup failed");
                        return None;
                    }
                };
                let key = match roles
                    .find_by_id(&mut scope, sanvi_access::domain::RoleId::from_uuid(role_id))
                    .await
                {
                    Ok(Some(role)) => Some(role.key().to_string()),
                    Ok(None) => None,
                    Err(err) => {
                        tracing::error!(error = %err, "role key lookup failed");
                        None
                    }
                };
                let _ = scope.commit().await;
                key
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        }
    };
    let access_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let access_consumer_metrics = Arc::new(ConsumerMetrics::new("access"));
    if let Err(err) = access_consumer_metrics.register(registry) {
        eprintln!("failed to register access consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let access_consumer = access_consumer.with_metrics(access_consumer_metrics);
    let access_consumer_name = format!("access-{}", uuid::Uuid::now_v7());
    let access_handler =
        sanvi_access::adapters::sync::access_sync_handler(access_store, None, role_key_resolver);

    // --- scheduler: purge sweep + idempotency sweep -------------------------
    let events = Arc::new(OutboxEventPublisher::new());
    let clock = Arc::new(SystemClock);
    let purge_sweep = Arc::new(PurgeSweep::new(
        tx_manager.clone(),
        tenants.clone(),
        events,
        clock,
        config.scheduler.purge_retention_days,
    ));
    let idempotency = Arc::new(sanvi_persistence::IdempotencyStore::new(pool.clone()));

    let jobs: Vec<Arc<dyn sanvi_messaging::Job>> = vec![
        Arc::new(FnJob::new(
            "tenancy.purge_sweep",
            Duration::from_secs(config.scheduler.purge_sweep_interval_secs),
            {
                let sweep = purge_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        sweep
                            .execute(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "platform.idempotency_sweep",
            Duration::from_secs(config.scheduler.idempotency_sweep_interval_secs),
            {
                let store = idempotency.clone();
                move || {
                    let store = store.clone();
                    Box::pin(async move {
                        let deleted = store.sweep_expired(Duration::from_secs(0)).await?;
                        if deleted > 0 {
                            tracing::info!(deleted, "idempotency sweep removed expired keys");
                        }
                        Ok(())
                    })
                }
            },
        )),
    ];
    let scheduler = Scheduler::new(
        redis.clone(),
        format!("worker-{}", uuid::Uuid::now_v7()),
        jobs,
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        sanvi_http::shutdown_signal().await;
        tracing::info!("shutdown signal received; draining worker");
        let _ = shutdown_tx.send(true);
    });

    tracing::info!(
        interval_ms = config.outbox.poll_interval_ms,
        "sanvi-worker starting",
    );

    let consumer_shutdown = shutdown_rx.clone();
    let tenancy_stream = format!("{}.tenancy", config.outbox.stream);
    let consumer_task = tokio::spawn(async move {
        consumer
            .run_until_shutdown(
                &tenancy_stream,
                "tenancy",
                &consumer_name,
                tenant_event_handler(invalidate),
                consumer_shutdown,
            )
            .await;
    });

    let access_shutdown = shutdown_rx.clone();
    let identity_stream = format!("{}.identity", config.outbox.stream);
    let access_task = tokio::spawn(async move {
        access_consumer
            .run_until_shutdown(
                &identity_stream,
                "access",
                &access_consumer_name,
                access_handler,
                access_shutdown,
            )
            .await;
    });

    let scheduler_shutdown = shutdown_rx.clone();
    let scheduler_task = tokio::spawn(async move {
        scheduler.run_until_shutdown(scheduler_shutdown).await;
    });

    let dispatcher_task = tokio::spawn(async move {
        dispatcher
            .run_until_shutdown(
                Duration::from_millis(config.outbox.poll_interval_ms),
                shutdown_rx,
            )
            .await;
    });

    let _ = tokio::join!(dispatcher_task, consumer_task, access_task, scheduler_task);

    telemetry_guard.shutdown();
    ExitCode::SUCCESS
}
