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
    FallbackGuideTranslator, TenantCatalogAdapter, build_access_state, build_app,
    build_audit_state, build_billing_state, build_domains_lookup, build_domains_state,
    build_identity_state, build_localization_state, build_payments_state,
    build_platform_admin_state, build_privacy_state, build_system_state, build_tenancy_state,
    build_theming_state,
};
use sanvi_http::{bind, run};
use sanvi_identity::domain::{RoleCatalog, TenantCatalog};
use sanvi_messaging::connect_redis;
use sanvi_persistence::build_pool;
use sanvi_privacy::adapters::PrivacyContextProvider;
use sanvi_privacy::adapters::repos::{
    SqlxConsentRepository, SqlxDirectiveRepository, SqlxDsrRepository, SqlxSignalRepository,
    SqlxSubjectRepository,
};
use sanvi_privacy::domain::ProviderRegistry;
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
        Ok(manager) => Some(manager),
        Err(err) => {
            // Redis is a dependency, not a boot blocker: readiness reports it
            // degraded and the rate limiter applies its fail-open policy.
            tracing::warn!(error = %err, "failed to connect to redis; continuing degraded");
            None
        }
    };

    let system = build_system_state(
        build_info(&config),
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
        redis.clone(),
        &config.tenancy,
        custom_domain_lookup,
    );

    // Build localization before notification-producing contexts so their
    // outbound adapters render through the shared override-aware service.
    let localization = if config.features.localization {
        Some(build_localization_state(
            pool.clone(),
            &config.localization,
            metrics.prometheus_registry(),
        ))
    } else {
        None
    };
    let notification_localizer = localization.as_ref().map(|wiring| {
        Arc::new(sanvi_localization::adapters::NotificationLocalizer::new(
            pool.clone(),
            wiring.service.clone(),
            config.localization.clone(),
        ))
    });

    // The role catalog bridges identity → access in-process; it needs only
    // a TxManager and the role repository, so there is no wiring cycle.
    let catalog_tx = sanvi_persistence::TxManager::new(pool.clone());
    let role_repository: Arc<dyn sanvi_access::domain::RoleRepository> =
        Arc::new(sanvi_access::adapters::SqlxRoleRepository::new());
    let role_catalog: Arc<dyn RoleCatalog> =
        Arc::new(SqlxRoleCatalog::new(catalog_tx, role_repository));

    // Same bridging shape as `role_catalog` above, one context over: `GET
    // /api/v1/me` needs each membership's tenant slug/display name, and
    // identity has no `sanvi-tenancy` dependency of its own.
    let tenant_catalog_tx = sanvi_persistence::TxManager::new(pool.clone());
    let tenant_repository_for_catalog: Arc<dyn sanvi_tenancy::domain::TenantRepository> =
        Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new());
    let tenant_catalog: Arc<dyn TenantCatalog> = Arc::new(TenantCatalogAdapter::new(
        tenant_catalog_tx,
        tenant_repository_for_catalog,
    ));

    let identity = build_identity_state(
        pool.clone(),
        redis.clone(),
        &config.identity,
        &config.ory.kratos_public_url,
        &config.ory.kratos_admin_url,
        role_catalog,
        tenant_catalog,
        notification_localizer.clone(),
    );

    // The audit writer/reader are shared by the access and platform_admin
    // contexts (they record; the audit context owns the schema).
    let audit = build_audit_state(pool.clone(), &config.audit);

    // Construction order matters: platform_admin owns the approval gate the
    // access context consumes, but its `EntitlementAdmin` port is *the
    // access context's own use cases*. So: build the approval gate first,
    // then access (which takes the gate), then wrap access's use cases into
    // the `EntitlementAdmin` adapter and finish platform_admin.
    let approval_gate = build_approval_gate(pool.clone());

    let access = build_access_state(
        pool.clone(),
        &config.access,
        &config.ory.keto_read_url,
        &config.ory.keto_write_url,
        identity.memberships.clone(),
        audit.writer.clone(),
        approval_gate.clone(),
        metrics.prometheus_registry(),
        redis.clone(),
    );

    let entitlement_admin: Arc<dyn sanvi_platform_admin::domain::EntitlementAdmin> =
        Arc::new(AccessEntitlementAdmin::new(
            access.grant_entitlement.clone(),
            access.revoke_entitlement.clone(),
            access.list_tenant_overrides.clone(),
        ));

    // Phase 05: the privacy lock is built first because the impersonation
    // flow consumes it; with the privacy feature off it stays a no-op.
    let privacy_tx = sanvi_persistence::TxManager::new(pool.clone());
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
        &config.platform_admin,
        Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
        identity.memberships.clone(),
        Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new()),
        entitlement_admin,
        audit.writer.clone(),
        privacy_lock.clone(),
    );

    // Phase 04: the billing context consumes the access context's
    // entitlement repository (subscription-source rows) and the audit
    // writer; its Stripe client and tax gate are built from BillingConfig.
    let billing = if config.features.billing {
        Some(build_billing_state(
            pool.clone(),
            &config.billing,
            access.entitlements_repo.clone(),
            access.features_repo.clone(),
            audit.writer.clone(),
            metrics.prometheus_registry(),
        ))
    } else {
        None
    };

    // Phase 05: every context crate registers a PersonalDataProvider; the
    // registry the DSR saga fans out over and the CI test proves complete.
    let privacy = if config.features.privacy {
        let privacy_providers: ProviderRegistry = vec![
            Arc::new(sanvi_system::adapters::SystemPrivacyProvider::new()),
            Arc::new(
                sanvi_tenancy::adapters::privacy_provider::TenancyPrivacyProvider::new(
                    privacy_tx.clone(),
                    Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
                ),
            ),
            Arc::new(
                sanvi_identity::adapters::privacy_provider::IdentityPrivacyProvider::new(
                    privacy_tx.clone(),
                    Arc::new(sanvi_identity::adapters::repo::SqlxUserRepository::new()),
                    Arc::new(sanvi_identity::adapters::repo::SqlxMembershipRepository::new()),
                    Arc::new(sanvi_identity::adapters::repo::SqlxLinkedAccountRepository::new()),
                    Arc::new(sanvi_identity::adapters::repo::SqlxInvitationRepository::new()),
                    Arc::new(sanvi_identity::adapters::repo::SqlxLinkIntentRepository::new()),
                    Arc::new(
                        sanvi_identity::adapters::HttpKratosClient::new(
                            config.ory.kratos_public_url.clone(),
                            Some(config.ory.kratos_admin_url.clone()),
                        )
                        .expect("kratos client construction is infallible"),
                    ),
                    Arc::new(sanvi_identity::adapters::SystemClock),
                ),
            ),
            Arc::new(
                sanvi_access::adapters::privacy_provider::AccessPrivacyProvider::new(
                    privacy_tx.clone(),
                    Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new()),
                    Arc::new(
                        sanvi_access::adapters::HttpKetoClient::new(
                            config.ory.keto_read_url.clone(),
                            config.ory.keto_write_url.clone(),
                        )
                        .expect("keto client construction is infallible"),
                    ),
                ),
            ),
            Arc::new(sanvi_audit::adapters::AuditPrivacyProvider::new(
                privacy_tx.clone(),
                Arc::new(sanvi_audit::adapters::SqlxAuditReader::new()),
                audit.writer.clone(),
            )),
            Arc::new(
                sanvi_platform_admin::adapters::privacy_provider::PlatformAdminPrivacyProvider::new(
                    privacy_tx.clone(),
                    Arc::new(
                        sanvi_platform_admin::adapters::SqlxImpersonationGrantRepository::new(),
                    ),
                    Arc::new(sanvi_platform_admin::adapters::SqlxPendingApprovalRepository::new()),
                    privacy_lock.clone(),
                ),
            ),
            Arc::new(
                sanvi_billing::adapters::privacy_provider::BillingPrivacyProvider::new(
                    privacy_tx.clone(),
                    Arc::new(sanvi_billing::adapters::SqlxSubscriptionRepository::new()),
                    Arc::new(sanvi_billing::adapters::SqlxCheckoutStateRepository::new()),
                    Arc::new(sanvi_billing::adapters::SqlxInvoiceRepository::new()),
                    Arc::new(sanvi_billing::adapters::SqlxWebhookEventRepository::new()),
                    Arc::new(sanvi_billing::adapters::HttpStripeClient::new(
                        config.billing.stripe_api_base.clone(),
                        config.billing.stripe_api_key.clone(),
                        config.billing.stripe_api_version.clone(),
                        None,
                    )),
                ),
            ),
            Arc::new(PrivacyContextProvider::new(
                privacy_tx.clone(),
                Arc::new(SqlxSubjectRepository::new()),
                Arc::new(SqlxConsentRepository::new()),
                Arc::new(SqlxSignalRepository::new()),
                Arc::new(SqlxDirectiveRepository::new()),
            )),
            Arc::new(
                sanvi_localization::adapters::LocalizationPrivacyProvider::new(privacy_tx.clone()),
            ),
            Arc::new(
                sanvi_theming::adapters::privacy_provider::ThemingPrivacyProvider::new(
                    privacy_tx.clone(),
                ),
            ),
            Arc::new(
                sanvi_domains::adapters::privacy_provider::DomainsPrivacyProvider::new(
                    privacy_tx.clone(),
                ),
            ),
            Arc::new(sanvi_payments::adapters::PaymentsPrivacyProvider::new()),
        ];
        Some(build_privacy_state(
            pool.clone(),
            &config.privacy,
            privacy_providers,
            audit.writer.clone(),
            notification_localizer.clone(),
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

    // Phase 09: tenant payment providers. Flag ships off — with it off no
    // payments route is mounted; with it on, the catalog route is
    // permission- and entitlement-gated (sub-phase 09.0).
    let payments = if config.features.payments {
        Some(build_payments_state(
            pool.clone(),
            &config.payments,
            metrics.prometheus_registry(),
        ))
    } else {
        None
    };

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
            redis.clone(),
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
        Arc::new(SystemIdGenerator),
        Arc::new(SystemClock),
    ));
    let _tx = sanvi_persistence::TxManager::new(pool);
    Arc::new(ApprovalGateAdapter::new(record))
}
