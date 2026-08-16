//! `sanvi-worker` — background worker: outbox dispatcher, stream consumers
//! and the leader-elected scheduler.
//!
//! - Outbox dispatcher relays unpublished domain events to per-context
//!   Redis Streams, dead-lettering rows that exhaust retries.
//! - Consumers process `sanvi.events.tenancy` (cache invalidation),
//!   `sanvi.events.identity` (Keto sync) and `sanvi.events.access`
//!   (entitlement cache invalidation, role → Keto tuple sync) with
//!   exactly-once semantics via `platform.processed_events`.
//! - The scheduler runs periodic jobs (purge sweep, idempotency sweep,
//!   quota rollover, entitlement expiry sweep, audit chain verification)
//!   on the elected leader only.
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
use sanvi_access::{
    AccessConfig,
    adapters::{
        HttpKetoClient, InMemoryDecisionCache, OutboxEventPublisher as AccessEventPublisher,
        RedisEntitlementCache, SqlxEntitlementRepository, SqlxQuotaUsageStore, SqlxRoleRepository,
        sync::entitlement_sync_handler,
    },
    application::ExpireEntitlements,
    domain::{
        Clock, EntitlementCache, EntitlementRepository, EventPublisher, PermissionStore,
        QuotaUsageStore, RoleRepository,
    },
};
use sanvi_audit::{
    AuditConfig,
    adapters::{SqlxAuditReader, SqlxAuditWriter},
    application::VerifyAuditChain,
};
use sanvi_billing::{
    BillingConfig,
    adapters::{
        AccessEntitlementSyncer, HttpStripeClient, LoggingNotifications,
        OutboxEventPublisher as BillingEventPublisher, SqlxInvoiceRepository,
        SqlxPlanEntitlementRepository, SqlxPlanPriceRepository, SqlxPlanRepository,
        SqlxSubscriptionRepository, SqlxWebhookEventRepository, StripeMetrics, SystemClock,
        SystemIdGenerator, TenancyTenantLifecycle, metrics::BillingMetrics,
    },
    application::{CheckTaxRegistrations, DunningSweep, ReconcileSubscriptions, WebhookDispatcher},
    domain::{
        Clock as BillingClock, EntitlementSyncer, EventPublisher as BillingEvents,
        IdGenerator as BillingIds, InvoiceRepository, Notifications, PlanEntitlementRepository,
        PlanPriceRepository, PlanRepository, StripeClient, SubscriptionRepository,
        WebhookEventRepository,
    },
};
use sanvi_messaging::{
    ConsumerMetrics, FnJob, OutboxDispatcher, OutboxMetrics, RedisConfig, Scheduler,
    StreamConsumer, StreamName, StreamsClient, connect_redis,
};
use sanvi_persistence::{DatabaseConfig, build_pool};
use sanvi_telemetry::TelemetryConfig;
use sanvi_tenancy::{
    TenancyConfig,
    adapters::{
        OutboxEventPublisher as TenancyEventPublisher, SystemClock as TenancySystemClock,
        events::tenant_event_handler, repo::SqlxTenantRepository,
        resolution::RedisTenantResolutionCache,
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
    /// Quota counter period rollover (drops usage rows older than the
    /// current period).
    pub quota_rollover_interval_secs: u64,
    /// Entitlement expiry sweep (deletes expired overrides + events).
    pub entitlement_expiry_sweep_interval_secs: u64,
    /// Audit hash-chain verification.
    pub audit_verify_interval_secs: u64,
    /// Stripe webhook event dispatch (phase 04).
    pub webhook_dispatch_interval_secs: u64,
    /// Dunning grace sweep (phase 04).
    pub dunning_sweep_interval_secs: u64,
    /// Stripe → local reconciliation (phase 04).
    pub reconciliation_interval_secs: u64,
    /// Stripe Tax registration check (phase 04).
    pub tax_check_interval_secs: u64,
    /// Billing gauges refresh (phase 04).
    pub billing_metrics_interval_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            purge_sweep_interval_secs: 3600,
            purge_retention_days: 30,
            idempotency_sweep_interval_secs: 3600,
            quota_rollover_interval_secs: 86400,
            entitlement_expiry_sweep_interval_secs: 3600,
            audit_verify_interval_secs: 86400,
            webhook_dispatch_interval_secs: 10,
            dunning_sweep_interval_secs: 3600,
            reconciliation_interval_secs: 86400,
            tax_check_interval_secs: 86400,
            billing_metrics_interval_secs: 60,
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
    pub access: AccessConfig,
    pub audit: AuditConfig,
    pub billing: BillingConfig,
}

impl WorkerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.database.validate()?;
        self.redis.validate()?;
        self.telemetry.validate()?;
        self.tenancy.validate()?;
        self.access.validate()?;
        self.audit.validate()?;
        self.billing.validate()?;
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
    let tenancy_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let tenancy_consumer_metrics = Arc::new(ConsumerMetrics::new("tenancy"));
    if let Err(err) = tenancy_consumer_metrics.register(registry) {
        eprintln!("failed to register tenancy consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let tenancy_consumer = tenancy_consumer.with_metrics(tenancy_consumer_metrics);
    let tenancy_consumer_name = format!("tenancy-{}", uuid::Uuid::now_v7());

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
    let access_handler = sanvi_access::adapters::sync::access_sync_handler(
        access_store.clone(),
        None,
        role_key_resolver,
    );

    // --- access events consumer: entitlement cache + role authoring sync ----
    let entitlements: Arc<dyn EntitlementRepository> = Arc::new(SqlxEntitlementRepository::new());
    let quota: Arc<dyn QuotaUsageStore> = Arc::new(SqlxQuotaUsageStore::new());
    let access_events: Arc<dyn EventPublisher> = Arc::new(AccessEventPublisher::new());
    let entitlement_cache: Arc<dyn EntitlementCache> = Arc::new(RedisEntitlementCache::new(
        redis.clone(),
        config.access.entitlement_cache_ttl_secs,
    ));

    let decision_cache: Arc<dyn sanvi_access::domain::DecisionCache> =
        Arc::new(InMemoryDecisionCache::new(10_000));
    let entitlement_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let entitlement_consumer_metrics = Arc::new(ConsumerMetrics::new("access-entitlements"));
    if let Err(err) = entitlement_consumer_metrics.register(registry) {
        eprintln!("failed to register access-entitlements consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let entitlement_consumer = entitlement_consumer.with_metrics(entitlement_consumer_metrics);
    let entitlement_consumer_name = format!("access-entitlements-{}", uuid::Uuid::now_v7());
    let entitlement_handler = entitlement_sync_handler(
        Some(entitlement_cache.clone()),
        Some(decision_cache),
        Some(access_store),
    );

    // --- billing context: Stripe webhooks, dunning, reconciliation -----------
    let billing_tx = sanvi_persistence::TxManager::new(pool.clone());
    let billing_tenancy_events = Arc::new(TenancyEventPublisher::new());
    let billing_plans: std::sync::Arc<dyn PlanRepository> =
        std::sync::Arc::new(SqlxPlanRepository::new());
    let billing_prices: std::sync::Arc<dyn PlanPriceRepository> =
        std::sync::Arc::new(SqlxPlanPriceRepository::new());
    let billing_grants: std::sync::Arc<dyn PlanEntitlementRepository> =
        std::sync::Arc::new(SqlxPlanEntitlementRepository::new());
    let billing_subscriptions: std::sync::Arc<dyn SubscriptionRepository> =
        std::sync::Arc::new(SqlxSubscriptionRepository::new());
    let billing_invoices: std::sync::Arc<dyn InvoiceRepository> =
        std::sync::Arc::new(SqlxInvoiceRepository::new());
    let billing_webhooks: std::sync::Arc<dyn WebhookEventRepository> =
        std::sync::Arc::new(SqlxWebhookEventRepository::new());
    let billing_events: std::sync::Arc<dyn BillingEvents> =
        std::sync::Arc::new(BillingEventPublisher::new());
    let billing_notifications: std::sync::Arc<dyn Notifications> =
        std::sync::Arc::new(LoggingNotifications::new());
    let billing_ids: std::sync::Arc<dyn BillingIds> = std::sync::Arc::new(SystemIdGenerator);
    let billing_clock: std::sync::Arc<dyn BillingClock> = std::sync::Arc::new(SystemClock);
    let billing_syncer: std::sync::Arc<dyn EntitlementSyncer> =
        std::sync::Arc::new(AccessEntitlementSyncer::new(entitlements.clone()));
    let billing_lifecycle: std::sync::Arc<dyn sanvi_billing::domain::TenantLifecycle> =
        std::sync::Arc::new(TenancyTenantLifecycle::new(
            billing_tx.clone(),
            tenants.clone(),
            billing_tenancy_events,
            std::sync::Arc::new(TenancySystemClock),
        ));
    let billing_metrics = BillingMetrics::new(registry);
    let stripe_metrics = StripeMetrics::new(registry);
    let billing_stripe: std::sync::Arc<dyn StripeClient> =
        std::sync::Arc::new(HttpStripeClient::new(
            config.billing.stripe_api_base.clone(),
            config.billing.stripe_api_key.clone(),
            config.billing.stripe_api_version.clone(),
            Some(stripe_metrics),
        ));
    let billing_dispatcher = std::sync::Arc::new(
        WebhookDispatcher::new(
            billing_tx.clone(),
            billing_webhooks.clone(),
            billing_subscriptions.clone(),
            billing_invoices.clone(),
            billing_prices.clone(),
            billing_plans.clone(),
            billing_grants.clone(),
            billing_syncer.clone(),
            billing_lifecycle.clone(),
            billing_notifications.clone(),
            billing_events.clone(),
            billing_stripe.clone(),
            billing_ids.clone(),
            billing_clock.clone(),
        )
        .with_metrics(billing_metrics.clone()),
    );
    let dunning_sweep = std::sync::Arc::new(DunningSweep::new(
        billing_tx.clone(),
        billing_subscriptions.clone(),
        billing_syncer.clone(),
        billing_lifecycle.clone(),
        billing_notifications.clone(),
        billing_events.clone(),
        billing_clock.clone(),
        config.billing.dunning_grace_days,
        config.billing.trial_reminder_days,
    ));
    let reconciliation = std::sync::Arc::new(ReconcileSubscriptions::new(
        billing_tx.clone(),
        billing_subscriptions.clone(),
        billing_stripe.clone(),
        billing_dispatcher.clone(),
    ));
    let tax_check = std::sync::Arc::new(CheckTaxRegistrations::new(
        billing_stripe.clone(),
        config.billing.automatic_tax_enabled,
    ));

    // --- billing events consumer: entitlement cache invalidation ------------
    let billing_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let billing_consumer_metrics = std::sync::Arc::new(ConsumerMetrics::new("billing"));
    if let Err(err) = billing_consumer_metrics.register(registry) {
        eprintln!("failed to register billing consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let billing_consumer = billing_consumer.with_metrics(billing_consumer_metrics);
    let billing_consumer_name = format!("billing-{}", uuid::Uuid::now_v7());
    let billing_event_handler =
        sanvi_billing::adapters::billing_event_handler(entitlement_cache.clone());

    // --- scheduler: purge + idempotency + quota rollover + expiry + audit ----
    let events = Arc::new(TenancyEventPublisher::new());
    let clock = Arc::new(TenancySystemClock);
    let purge_sweep = Arc::new(PurgeSweep::new(
        tx_manager.clone(),
        tenants.clone(),
        events,
        clock,
        config.scheduler.purge_retention_days,
    ));
    let idempotency = Arc::new(sanvi_persistence::IdempotencyStore::new(pool.clone()));
    let expire_entitlements = Arc::new(ExpireEntitlements::new(
        tx_manager.clone(),
        entitlements.clone(),
        entitlement_cache.clone(),
        access_events.clone(),
        Arc::new(sanvi_access::adapters::SystemClock),
    ));
    let audit_reader = Arc::new(SqlxAuditReader::new());
    let _audit_writer = SqlxAuditWriter::new();
    let verify_audit = Arc::new(VerifyAuditChain::new(
        tx_manager.clone(),
        audit_reader,
        config.audit.verify_batch_size,
    ));

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
        Arc::new(FnJob::new(
            "access.quota_rollover",
            Duration::from_secs(config.scheduler.quota_rollover_interval_secs),
            {
                let quota = quota.clone();
                let tx = tx_manager.clone();
                move || {
                    let quota = quota.clone();
                    let tx = tx.clone();
                    Box::pin(async move {
                        // Keep only the current period: usage resets monthly.
                        let now = sanvi_access::domain::period_start(
                            sanvi_access::adapters::SystemClock.now(),
                        );
                        let mut scope = tx.begin_unscoped("quota_rollover", "access").await?;
                        let removed = quota.rollover(&mut scope, now).await?;
                        scope.commit().await?;
                        if removed > 0 {
                            tracing::info!(removed, "quota rollover removed old periods");
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "access.entitlement_expiry_sweep",
            Duration::from_secs(config.scheduler.entitlement_expiry_sweep_interval_secs),
            {
                let sweep = expire_entitlements.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let expired = sweep
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if expired > 0 {
                            tracing::info!(expired, "entitlement sweep removed expired overrides");
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "audit.verify_chain",
            Duration::from_secs(config.scheduler.audit_verify_interval_secs),
            {
                let verify = verify_audit.clone();
                move || {
                    let verify = verify.clone();
                    Box::pin(async move {
                        let report = verify.execute().await.map_err(|err| anyhow::anyhow!(err))?;
                        if report.verified {
                            tracing::info!(
                                entries = report.entries_checked,
                                "audit chain verified"
                            );
                        } else {
                            // A tampered chain is a security incident: alert
                            // immediately (the log level + metric fire the
                            // alerting rule).
                            tracing::error!(
                                entries = report.entries_checked,
                                mismatch = %report.first_mismatch.unwrap_or_default(),
                                "AUDIT CHAIN VERIFICATION FAILED — possible tampering"
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        // --- phase 04: billing -------------------------------------------------
        Arc::new(FnJob::new(
            "billing.webhook_dispatch",
            Duration::from_secs(config.scheduler.webhook_dispatch_interval_secs),
            {
                let dispatcher = billing_dispatcher.clone();
                move || {
                    let dispatcher = dispatcher.clone();
                    Box::pin(async move {
                        let report = dispatcher
                            .dispatch(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.processed > 0 || report.skipped > 0 || report.failed > 0 {
                            tracing::info!(
                                processed = report.processed,
                                skipped = report.skipped,
                                failed = report.failed,
                                "stripe webhook dispatch pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "billing.dunning_sweep",
            Duration::from_secs(config.scheduler.dunning_sweep_interval_secs),
            {
                let sweep = dunning_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.grace_expired > 0 || report.trials_notified > 0 {
                            tracing::info!(
                                grace_expired = report.grace_expired,
                                trials_notified = report.trials_notified,
                                "billing dunning sweep",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "billing.reconciliation",
            Duration::from_secs(config.scheduler.reconciliation_interval_secs),
            {
                let reconciliation = reconciliation.clone();
                let metrics = billing_metrics.clone();
                move || {
                    let reconciliation = reconciliation.clone();
                    let metrics = metrics.clone();
                    Box::pin(async move {
                        let report = reconciliation
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        metrics.drift_total().inc_by(report.drift_repaired as u64);
                        tracing::info!(
                            checked = report.checked,
                            drift_repaired = report.drift_repaired,
                            errors = report.errors,
                            "billing reconciliation pass",
                        );
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "billing.tax_check",
            Duration::from_secs(config.scheduler.tax_check_interval_secs),
            {
                let tax_check = tax_check.clone();
                move || {
                    let tax_check = tax_check.clone();
                    Box::pin(async move {
                        // Errors already log at error level inside the use
                        // case (the alerting rule fires on them).
                        let _ = tax_check.execute().await;
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "billing.metrics",
            Duration::from_secs(config.scheduler.billing_metrics_interval_secs),
            {
                let tx = billing_tx.clone();
                let subscriptions = billing_subscriptions.clone();
                let webhooks = billing_webhooks.clone();
                let metrics = billing_metrics.clone();
                move || {
                    let tx = tx.clone();
                    let subscriptions = subscriptions.clone();
                    let webhooks = webhooks.clone();
                    let metrics = metrics.clone();
                    Box::pin(async move {
                        let mut scope = tx
                            .begin_unscoped("billing_metrics", "worker")
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        metrics
                            .unprocessed()
                            .set(webhooks.unprocessed_count(&mut scope).await? as i64);
                        metrics
                            .dead_letter_total()
                            .set(webhooks.dead_letter_count(&mut scope).await? as i64);
                        metrics
                            .dunning_active()
                            .set(subscriptions.dunning_count(&mut scope).await? as i64);
                        metrics
                            .mrr()
                            .set(subscriptions.mrr_minor(&mut scope).await? as f64);
                        for (status, count) in subscriptions.count_by_status(&mut scope).await? {
                            metrics
                                .status_gauge()
                                .with_label_values(&[status.as_str()])
                                .set(count);
                        }
                        scope.commit().await.map_err(|err| anyhow::anyhow!(err))?;
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

    let tenancy_shutdown = shutdown_rx.clone();
    let tenancy_stream = format!("{}.tenancy", config.outbox.stream);
    let tenancy_task = tokio::spawn(async move {
        tenancy_consumer
            .run_until_shutdown(
                &tenancy_stream,
                "tenancy",
                &tenancy_consumer_name,
                tenant_event_handler(invalidate),
                tenancy_shutdown,
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

    let entitlement_shutdown = shutdown_rx.clone();
    let access_stream = format!("{}.access", config.outbox.stream);
    let entitlement_task = tokio::spawn(async move {
        entitlement_consumer
            .run_until_shutdown(
                &access_stream,
                "access-entitlements",
                &entitlement_consumer_name,
                entitlement_handler,
                entitlement_shutdown,
            )
            .await;
    });

    let billing_shutdown = shutdown_rx.clone();
    let billing_stream = format!("{}.billing", config.outbox.stream);
    let billing_task = tokio::spawn(async move {
        billing_consumer
            .run_until_shutdown(
                &billing_stream,
                "billing",
                &billing_consumer_name,
                billing_event_handler,
                billing_shutdown,
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

    let _ = tokio::join!(
        dispatcher_task,
        tenancy_task,
        access_task,
        entitlement_task,
        billing_task,
        scheduler_task
    );

    telemetry_guard.shutdown();
    ExitCode::SUCCESS
}
