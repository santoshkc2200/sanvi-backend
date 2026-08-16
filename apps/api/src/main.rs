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
    AccessEntitlementAdmin, AppConfig, build_access_state, build_app, build_audit_state,
    build_identity_state, build_platform_admin_state, build_system_state, build_tenancy_state,
};
use sanvi_http::{bind, run};
use sanvi_identity::domain::RoleCatalog;
use sanvi_messaging::connect_redis;
use sanvi_persistence::build_pool;
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

    let tenancy = build_tenancy_state(pool.clone(), redis.clone(), &config.tenancy);

    // The role catalog bridges identity → access in-process; it needs only
    // a TxManager and the role repository, so there is no wiring cycle.
    let catalog_tx = sanvi_persistence::TxManager::new(pool.clone());
    let role_repository: Arc<dyn sanvi_access::domain::RoleRepository> =
        Arc::new(sanvi_access::adapters::SqlxRoleRepository::new());
    let role_catalog: Arc<dyn RoleCatalog> =
        Arc::new(SqlxRoleCatalog::new(catalog_tx, role_repository));

    let identity = build_identity_state(
        pool.clone(),
        redis.clone(),
        &config.identity,
        &config.ory.kratos_public_url,
        &config.ory.kratos_admin_url,
        role_catalog,
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

    let platform_admin = build_platform_admin_state(
        pool.clone(),
        &config.platform_admin,
        Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
        identity.memberships.clone(),
        Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new()),
        entitlement_admin,
        audit.writer.clone(),
    );

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
