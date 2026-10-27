//! `sanvi-api` binary: composition root.
//!
//! Reads config → initialises telemetry → connects Postgres/Redis → injects
//! adapters into the system context's use cases → serves with graceful
//! shutdown (SIGTERM/SIGINT: stop accepting, drain in-flight requests within
//! the configured window, close pools).

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use sanvi_access::adapters::SqlxRoleCatalog;
use sanvi_api::{
    AccessEntitlementAdmin, AppConfig, DomainsGuideTranslatorAdapter, DomainsTenantLookupAdapter,
    FallbackGuideTranslator, IdentityDependencies, PlatformAdminDependencies, TenantCatalogAdapter,
    build_access_state, build_advertising_state, build_app, build_audit_state, build_billing_state,
    build_domains_lookup, build_domains_state, build_identity_state, build_localization_state,
    build_payments_state, build_platform_admin_state, build_privacy_state, build_system_state,
    build_tenancy_state, build_theming_state,
};
use sanvi_http::{bind, run};
use sanvi_identity::domain::{RoleCatalog, TenantCatalog};
use sanvi_kernel::{RawIdGenerator, SnowflakeGenerator};
use sanvi_messaging::connect_redis;
use sanvi_persistence::build_pool;
use sanvi_privacy::adapters::repos::SqlxDsrRepository;
use sanvi_system::domain::BuildInfo;

/// Load configuration: `configs/base.toml` → `configs/{env}.toml` (selected
/// by `SANVI_ENV`) → environment overrides (`SANVI__` prefix, `__` nesting).
fn load_config() -> anyhow::Result<AppConfig> {
    let environment = std::env::var("SANVI_ENV").unwrap_or_else(|_| "local".to_string());
    let base = "configs/base.toml";
    let overlay = format!("configs/{environment}.toml");

    let config: AppConfig = Figment::new()
        .merge(Toml::file(base))
        .merge(Toml::file(overlay))
        .merge(Env::prefixed("SANVI__").split("__"))
        .extract()
        .context("failed to load configuration")?;

    config.validate()?;
    config.ids.resolve(&config.telemetry.environment, 1)?;
    Ok(config)
}

fn build_info(config: &AppConfig) -> BuildInfo {
    BuildInfo::new(
        env!("CARGO_PKG_VERSION").to_string(),
        option_env!("GIT_COMMIT").unwrap_or("unknown").to_string(),
        option_env!("BUILD_TIME").unwrap_or("unknown").to_string(),
        option_env!("RUST_VERSION")
            .unwrap_or(env!("CARGO_PKG_RUST_VERSION"))
            .to_string(),
        config.telemetry.service_name.clone(),
    )
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

    let node_id = match config.ids.resolve(&config.telemetry.environment, 1) {
        Ok(node_id) => node_id,
        Err(err) => {
            eprintln!("configuration error:\n{err:#}");
            return ExitCode::from(2);
        }
    };
    let ids: Arc<dyn RawIdGenerator> = Arc::new(SnowflakeGenerator::new(node_id));

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

    if let Err(err) = sanvi_privacy::adapters::erasure_ledger::require_replayed(
        &sanvi_persistence::TxManager::new(pool.clone(), ids.clone()),
        &config.privacy,
    )
    .await
    {
        eprintln!("privacy recovery gate failed: {err}");
        return ExitCode::from(1);
    }

    // TASK-021 step 0: pool gauges plus the 1 Hz acquire probe, so pool
    // saturation is measured (step 5 sizes the pools from these numbers).
    // TASK-023 step 6: the probe also feeds the shedder's live pressure
    // signal, so pool saturation sheds load instead of queueing it.
    let pool_pressure = std::sync::Arc::new(sanvi_resilience::PoolPressure::new());
    let pool_metrics = std::sync::Arc::new(
        sanvi_persistence::PoolMetrics::new(metrics.prometheus_registry())
            .with_pressure(pool_pressure.clone()),
    );
    pool_metrics.spawn_sampler(pool.clone());

    // TASK-023: the degradation registry is process-wide and always built
    // (adapters flip reasons on it whether or not the header flag is on);
    // the `platform.degraded_mode` flag only decides whether the header
    // middleware stamps responses.
    let degradation = std::sync::Arc::new(sanvi_resilience::DegradationRegistry::new(Some(
        metrics.prometheus_registry(),
    )));
    let load_shed = Arc::new(
        sanvi_http::LoadShedState::new(config.server.load_shedding.clone(), Some(pool_pressure))
            .with_degradation(degradation.clone()),
    );

    let redis = match connect_redis(&config.redis).await {
        Ok(manager) => Some(manager),
        Err(err) => {
            // Redis is a dependency, not a boot blocker: readiness reports it
            // degraded and the rate limiter applies its fail-open policy.
            tracing::warn!(error = %err, "failed to connect to redis; continuing degraded");
            None
        }
    };
    // The health signal runs independently of rate limiting and admission.
    // A managed connection reconnects after an outage; no admitted request
    // is required to restore the full ceiling.
    sanvi_http::rate_limit::probe_redis(redis.clone(), &degradation).await;
    let probe_conn = redis.clone();
    let probe_registry = degradation.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            sanvi_http::rate_limit::probe_redis(probe_conn.clone(), &probe_registry).await;
        }
    });

    let system = build_system_state(
        build_info(&config),
        &config.telemetry.environment,
        pool.clone(),
        redis.clone(),
        config.readiness_probe_secs,
    );

    // Phase 08: the custom-domain lookup hook is installed before tenancy
    // builds its resolver (resolution order: signed header → custom domain
    // → subdomain).
    let domains_lookup = build_domains_lookup(pool.clone(), redis.clone(), &config.domains);
    let custom_domain_lookup: Option<
        std::sync::Arc<dyn sanvi_tenancy::adapters::resolution::CustomDomainLookup>,
    > = config.features.domains.then(|| {
        std::sync::Arc::new(DomainsTenantLookupAdapter::new(domains_lookup.clone()))
            as std::sync::Arc<dyn sanvi_tenancy::adapters::resolution::CustomDomainLookup>
    });
    let tenancy = build_tenancy_state(
        pool.clone(),
        ids.clone(),
        redis.clone(),
        &config.tenancy,
        custom_domain_lookup,
    );

    // Build localization before notification-producing contexts so their
    // outbound adapters render through the shared override-aware service.
    let localization = if config.features.localization {
        Some(build_localization_state(
            pool.clone(),
            ids.clone(),
            &config.localization,
            metrics.prometheus_registry(),
        ))
    } else {
        None
    };
    let notification_localizer = localization.as_ref().map(|wiring| {
        Arc::new(sanvi_localization::adapters::NotificationLocalizer::new(
            pool.clone(),
            ids.clone(),
            wiring.service.clone(),
            config.localization.clone(),
        ))
    });

    // Delivery for the two credential-bearing (Lane B) senders: invitations
    // and DSR verification/export mail. Everything else is delivered by the
    // worker's outbox consumers. An empty base_url leaves this `None` and
    // those adapters fall back to logging. The gateway counts every send
    // against the tenant's abuse window (TASK-024, FR-1117); the API and
    // worker gateways share the Redis key namespace, so both lanes count
    // into the same window.
    let notification_gateway: Option<Arc<dyn sanvi_notifications::NotificationGatewayPort>> =
        if config.features.notifications && !config.notification.base_url.is_empty() {
            Some(Arc::new(
                sanvi_notifications::HttpNotificationGateway::new(config.notification.clone())
                    .with_abuse_counter(
                        Arc::new(sanvi_http::abuse::AbuseCounter::new(redis.clone())),
                        config.notification.send_quota_limit,
                        config.notification.send_quota_window_secs,
                    )
                    .with_commercial_quota(sanvi_http::quota_middleware::QuotaRuntime {
                        service: tenancy
                            .quotas
                            .as_ref()
                            .expect("production tenancy owns counters")
                            .clone(),
                        enabled: config.platform.quotas,
                    }),
            ))
        } else {
            tracing::info!("notification service not configured; Lane B mail is logged only");
            None
        };

    // The role catalog bridges identity → access in-process; it needs only
    // a TxManager and the role repository, so there is no wiring cycle.
    let catalog_tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let role_repository: Arc<dyn sanvi_access::domain::RoleRepository> =
        Arc::new(sanvi_access::adapters::SqlxRoleRepository::new());
    let role_catalog: Arc<dyn RoleCatalog> =
        Arc::new(SqlxRoleCatalog::new(catalog_tx, role_repository));

    // Same bridging shape as `role_catalog` above, one context over: `GET
    // /api/v1/me` needs each membership's tenant slug/display name, and
    // identity has no `sanvi-tenancy` dependency of its own.
    let tenant_catalog_tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let tenant_repository_for_catalog: Arc<dyn sanvi_tenancy::domain::TenantRepository> =
        Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new());
    let tenant_catalog: Arc<dyn TenantCatalog> = Arc::new(TenantCatalogAdapter::new(
        tenant_catalog_tx,
        tenant_repository_for_catalog,
    ));

    let identity = build_identity_state(
        pool.clone(),
        ids.clone(),
        redis.clone(),
        &config.identity,
        &config.ory.kratos_public_url,
        &config.ory.kratos_admin_url,
        IdentityDependencies {
            role_catalog,
            tenant_catalog,
            notification_localizer: notification_localizer.clone(),
            notification_gateway: notification_gateway.clone(),
        },
    );

    // The audit writer/reader are shared by the access and platform_admin
    // contexts (they record; the audit context owns the schema).
    let audit = build_audit_state(
        pool.clone(),
        ids.clone(),
        &config.audit,
        config.features.archival,
    );

    // Construction order matters: platform_admin owns the approval gate the
    // access context consumes, but its `EntitlementAdmin` port is *the
    // access context's own use cases*. So: build the approval gate first,
    // then access (which takes the gate), then wrap access's use cases into
    // the `EntitlementAdmin` adapter and finish platform_admin.
    let approval_gate = build_approval_gate(pool.clone(), ids.clone());

    let access = build_access_state(
        pool.clone(),
        ids.clone(),
        &config.access,
        &config.ory.keto_read_url,
        &config.ory.keto_write_url,
        identity.memberships.clone(),
        audit.writer.clone(),
        approval_gate.clone(),
        metrics.prometheus_registry(),
        redis.clone(),
        degradation.clone(),
    );

    let entitlement_admin: Arc<dyn sanvi_platform_admin::domain::EntitlementAdmin> =
        Arc::new(AccessEntitlementAdmin::new(
            access.grant_entitlement.clone(),
            access.revoke_entitlement.clone(),
            access.list_tenant_overrides.clone(),
        ));

    // Phase 05: the privacy lock is built first because the impersonation
    // flow consumes it; with the privacy feature off it stays a no-op.
    let privacy_tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let privacy_requests: Arc<dyn sanvi_privacy::domain::DsrRepository> =
        Arc::new(SqlxDsrRepository::new());
    let privacy_lock: Arc<dyn sanvi_platform_admin::domain::PrivacyLock> =
        if config.features.privacy {
            Arc::new(sanvi_api::DsrPrivacyLock::new(
                privacy_tx.clone(),
                privacy_requests.clone(),
            ))
        } else {
            Arc::new(sanvi_platform_admin::adapters::NoopPrivacyLock)
        };

    let platform_admin = build_platform_admin_state(
        pool.clone(),
        ids.clone(),
        &config.platform_admin,
        PlatformAdminDependencies {
            tenants: Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
            memberships: identity.memberships.clone(),
            entitlements: Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new()),
            entitlement_admin,
            audit_writer: audit.writer.clone(),
            privacy_lock: privacy_lock.clone(),
        },
    );

    // Phase 04: the billing context consumes the access context's
    // entitlement repository (subscription-source rows) and the audit
    // writer; its Stripe client and tax gate are built from BillingConfig.
    let billing = if config.features.billing {
        Some(build_billing_state(
            pool.clone(),
            ids.clone(),
            &config.billing,
            access.entitlements_repo.clone(),
            access.features_repo.clone(),
            audit.writer.clone(),
            metrics.prometheus_registry(),
            degradation.clone(),
        ))
    } else {
        None
    };

    // Phase 05: every context crate registers a PersonalDataProvider; the
    // registry the DSR saga fans out over and the CI test proves complete.
    // Built by the shared `privacy_provider_registry` so the API, the CLI
    // drills and the compliance test crate cannot drift apart.
    let privacy = if config.features.privacy {
        let privacy_providers =
            sanvi_api::privacy_provider_registry(&sanvi_api::PrivacyProviderDeps {
                media: config.media.clone(),
                product_access: config.product_access.clone(),
                tx: privacy_tx.clone(),
                kratos_public_url: config.ory.kratos_public_url.clone(),
                kratos_admin_url: config.ory.kratos_admin_url.clone(),
                keto_read_url: config.ory.keto_read_url.clone(),
                keto_write_url: config.ory.keto_write_url.clone(),
                session_cookie_name: config.identity.session_cookie_name.clone(),
                stripe_api_base: config.billing.stripe_api_base.clone(),
                stripe_api_key: config.billing.stripe_api_key.clone(),
                stripe_api_version: config.billing.stripe_api_version.clone(),
                audit_writer: audit.writer.clone(),
                privacy_lock: privacy_lock.clone(),
            });
        Some(build_privacy_state(
            pool.clone(),
            ids.clone(),
            &config.privacy,
            privacy_providers,
            audit.writer.clone(),
            notification_localizer.clone(),
            notification_gateway.clone(),
        ))
    } else {
        None
    };

    // Phase 07: theme registry, tenant drafts/publish/rollback and the
    // resolved-theme delivery path. The entitlement gate rides on the
    // access context's domain gate; tenant resolution for the public
    // endpoint rides on the tenancy context's host resolver.
    let theming = if config.features.theming {
        Some(build_theming_state(
            pool.clone(),
            ids.clone(),
            &config.theming,
            metrics.prometheus_registry(),
            redis.clone(),
            access.domain_feature_gate.clone(),
            tenancy.host_resolver.clone(),
        ))
    } else {
        None
    };

    // Phase 08: custom domains. The guide translator rides on the
    // localization context (off → raw keys); the entitlement gate rides on
    // the access context; tenant activity rides on tenancy's resolver.
    let domains = if config.features.domains {
        let translator: std::sync::Arc<dyn sanvi_domains::domain::GuideTranslator> =
            match &localization {
                Some(wiring) => std::sync::Arc::new(DomainsGuideTranslatorAdapter::new(
                    wiring.translator.clone(),
                )),
                None => std::sync::Arc::new(FallbackGuideTranslator),
            };
        Some(build_domains_state(
            pool.clone(),
            ids.clone(),
            &config.domains,
            metrics.prometheus_registry(),
            redis.clone(),
            access.domain_feature_gate.clone(),
            translator,
            domains_lookup.clone(),
            tenancy.resolve.clone(),
        ))
    } else {
        None
    };

    // Phase 09: build the webhook receiver even when the tenant-facing
    // surface is rolled back. Stripe can still complete a session created
    // before the switch; that money must retain a local record.
    let payments = Some(build_payments_state(
        pool.clone(),
        ids.clone(),
        &config.payments,
        metrics.prometheus_registry(),
        &config.telemetry.environment,
        audit.writer.clone(),
    ));
    let advertising = if config.features.advertising {
        // The directive gate (TASK-014) is advertising's core invariant —
        // there is no meaningful way to run this context without the
        // privacy context's resolver to ask.
        let Some(directive_resolver) = privacy.as_ref().map(|wiring| wiring.resolver.clone())
        else {
            eprintln!("failed to configure advertising: the privacy feature must be enabled");
            return ExitCode::from(1);
        };
        let subprocessors: Arc<dyn sanvi_privacy::domain::SubProcessorRepository> =
            Arc::new(sanvi_privacy::adapters::SqlxSubProcessorRepository::new());
        match build_advertising_state(
            pool.clone(),
            ids.clone(),
            &config.advertising,
            directive_resolver,
            subprocessors,
        )
        .await
        {
            Ok(state) => Some(state),
            Err(err) => {
                eprintln!("failed to configure advertising: {err}");
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };

    // TASK-022 (FR-1111): one SSR cache per process — see build_app's note.
    // Registered counters land here, once, before the workers fork.
    let ssr_cache = config.server.ssr_cache.enabled.then(|| {
        let ssr_metrics = sanvi_cache::SsrCacheMetrics::new(metrics.prometheus_registry());
        std::sync::Arc::new(sanvi_cache::SsrCache::new(redis.clone(), Some(ssr_metrics)))
    });

    let bind_config = config.clone();
    let factory_config = config.clone();
    let server = match bind(&bind_config.server, move || {
        build_app(
            factory_config.clone(),
            metrics.clone(),
            system.clone(),
            tenancy.clone(),
            identity.clone(),
            access.clone(),
            audit.clone(),
            platform_admin.clone(),
            billing.clone(),
            privacy.clone(),
            localization.clone(),
            theming.clone(),
            domains.clone(),
            payments.clone(),
            advertising.clone(),
            redis.clone(),
            ssr_cache.clone(),
            degradation.clone(),
            Some(load_shed.clone()),
        )
    }) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("failed to bind http server: {err}");
            return ExitCode::from(1);
        }
    };

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        host = %config.server.host,
        port = config.server.port,
        "sanvi-api starting",
    );

    run(server).await;

    telemetry_guard.shutdown();
    ExitCode::SUCCESS
}

/// The four-eyes approval gate: the access context's `GrantEntitlement`
/// records pending approvals through it. Built before access because
/// access consumes it.
fn build_approval_gate(
    pool: sanvi_persistence::PgPool,
    ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
) -> Arc<dyn sanvi_access::domain::ApprovalGate> {
    use sanvi_platform_admin::{
        adapters::{
            ApprovalGateAdapter, SqlxPendingApprovalRepository, SystemClock, SystemIdGenerator,
        },
        application::approvals::RecordApprovalRequest,
    };
    let approvals = Arc::new(SqlxPendingApprovalRepository::new());
    let record = Arc::new(RecordApprovalRequest::new(
        approvals,
        Arc::new(SystemIdGenerator::new(ids.clone())),
        Arc::new(SystemClock),
    ));
    let _tx = sanvi_persistence::TxManager::new(pool, ids);
    Arc::new(ApprovalGateAdapter::new(record))
}
