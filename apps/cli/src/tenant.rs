//! `tenant` — operator commands for tenant provisioning and lifecycle.
//!
//! Until self-service lands (phases 03/04), an operator creates tenants from
//! here. Every command runs against the real application services — same
//! validation, same RLS scoping, same outbox writes as the HTTP path.

use std::sync::Arc;

use anyhow::Context;
use sanvi_persistence::TxManager;
use sanvi_tenancy::{
    adapters::{
        OutboxEventPublisher, SystemClock, SystemIdGenerator,
        repo::{SqlxSlugReservationPort, SqlxTenantRepository},
    },
    application::{
        ActivateTenant, ArchiveTenant, GetTenant, ListTenants, ProvisionTenant,
        ProvisionTenantCommand, ResumeTenant, SuspendTenant,
    },
    domain::{
        EventPublisher, IdGenerator, SlugReservationPort, TenantId, TenantRepository, TenantStatus,
    },
};
use serde::Deserialize;

use crate::config::load as load_config;

/// Services shared by every tenant command.
struct Services {
    provision: ProvisionTenant,
    activate: ActivateTenant,
    suspend: SuspendTenant,
    resume: ResumeTenant,
    archive: ArchiveTenant,
    get_tenant: GetTenant,
    list_tenants: ListTenants,
}

async fn services(environment: &str) -> anyhow::Result<Services> {
    let config = load_config(environment)?;
    let pool = sanvi_persistence::build_pool(&config.database).await?;
    let tx_manager = TxManager::new(pool);

    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let slugs: Arc<dyn SlugReservationPort> = Arc::new(SqlxSlugReservationPort::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    let ids: Arc<dyn IdGenerator> = Arc::new(SystemIdGenerator);
    let clock: Arc<dyn sanvi_tenancy::domain::Clock> = Arc::new(SystemClock);

    Ok(Services {
        provision: ProvisionTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            slugs,
            events.clone(),
            ids,
            clock.clone(),
        ),
        activate: ActivateTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        ),
        suspend: SuspendTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        ),
        resume: ResumeTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        ),
        archive: ArchiveTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        ),
        get_tenant: GetTenant::new(tx_manager.clone(), tenants.clone()),
        list_tenants: ListTenants::new(tx_manager, tenants),
    })
}

#[derive(Debug, Deserialize)]
pub struct TenantCreateArgs {
    pub slug: String,
    pub display_name: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default = "default_locale")]
    pub default_locale: String,
}

fn default_region() -> String {
    "us".to_string()
}

fn default_locale() -> String {
    "en".to_string()
}

/// `tenant create`: provision (and optionally activate) a tenant.
pub async fn create(
    environment: &str,
    args: &TenantCreateArgs,
    activate: bool,
) -> anyhow::Result<()> {
    let services = services(environment).await?;
    let view = services
        .provision
        .execute(ProvisionTenantCommand {
            slug: args.slug.clone(),
            display_name: args.display_name.clone(),
            region: args.region.clone(),
            default_locale: args.default_locale.clone(),
        })
        .await
        .context("provisioning failed")?;

    let view = if activate {
        let id: TenantId = view.id.parse().context("invalid tenant id returned")?;
        services
            .activate
            .execute(id)
            .await
            .context("activation failed")?
    } else {
        view
    };

    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}

/// `tenant list`: cursor-paginated listing with optional status filter.
pub async fn list(
    environment: &str,
    after: Option<String>,
    limit: u32,
    status: Option<String>,
) -> anyhow::Result<()> {
    let services = services(environment).await?;
    let status = match status {
        Some(raw) => Some(TenantStatus::parse(&raw).map_err(|err| anyhow::anyhow!("{err}"))?),
        None => None,
    };
    let after = match after {
        Some(raw) => Some(raw.parse().context("invalid cursor")?),
        None => None,
    };
    let page = services
        .list_tenants
        .execute(after, limit, status)
        .await
        .context("listing tenants failed")?;
    println!("{}", serde_json::to_string_pretty(&page)?);
    Ok(())
}

/// `tenant get <id>`: print one tenant.
pub async fn get(environment: &str, id: &str) -> anyhow::Result<()> {
    let services = services(environment).await?;
    let id: TenantId = id.parse().context("invalid tenant id")?;
    let view = services
        .get_tenant
        .execute(id)
        .await
        .context("get failed")?;
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}

/// `tenant suspend|resume|archive|activate <id>`: lifecycle transitions.
pub async fn lifecycle(
    environment: &str,
    id: &str,
    action: LifecycleCommand,
    reason: Option<String>,
) -> anyhow::Result<()> {
    let services = services(environment).await?;
    let id: TenantId = id.parse().context("invalid tenant id")?;
    let view = match action {
        LifecycleCommand::Activate => services
            .activate
            .execute(id)
            .await
            .context("activate failed")?,
        LifecycleCommand::Resume => services.resume.execute(id).await.context("resume failed")?,
        LifecycleCommand::Archive => services
            .archive
            .execute(id)
            .await
            .context("archive failed")?,
        LifecycleCommand::Suspend => {
            let reason = reason.context("--reason is required for suspend")?;
            let reason = sanvi_tenancy::SuspensionReason::parse(&reason)
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            services
                .suspend
                .execute(id, reason)
                .await
                .context("suspend failed")?
        }
    };
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleCommand {
    Activate,
    Suspend,
    Resume,
    Archive,
}
