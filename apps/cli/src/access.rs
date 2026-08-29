//! `seed` + `membership` — operator commands for the access context and
//! platform role assignment.
//!
//! - `seed roles` — idempotently insert the system role catalog.
//! - `seed access` — rebuild every Keto relation tuple from the role
//!   catalog and all memberships (the reconciliation path; safe to rerun).
//! - `membership grant` — make a user a member of a tenant with roles, or
//!   assign a platform role (platform assignments live in Keto only until
//!   phase 03 gives them a database home).

use std::sync::Arc;

use sanvi_access::{
    adapters::{HttpKetoClient, SqlxMembershipSource, SqlxRoleCatalog, SqlxRoleRepository},
    application::{RebuildAccessTuples, SeedFeatures, SeedRoles},
    domain::{Clock, IdGenerator, PermissionStore, RoleRepository, Tuple},
};
use sanvi_identity::{
    adapters::{
        OutboxEventPublisher, SystemClock,
        repo::{SqlxMembershipRepository, SqlxUserRepository},
    },
    application::{GrantMembership, GrantMembershipCommand},
    domain::{Email, EventPublisher, MembershipRepository, RoleCatalog, UserRepository},
};
use sanvi_kernel::RawIdGenerator;
use sanvi_persistence::TxManager;
use sanvi_tenancy::domain::TenantRepository as _;

use crate::config::load as load_config;

#[derive(Debug, Clone, Copy)]
pub enum SeedKind {
    Roles,
    Access,
    Features,
}

pub async fn seed(
    environment: &str,
    kind: SeedKind,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let config = load_config(environment)?;
    let pool = sanvi_persistence::build_pool(&config.database).await?;
    let tx_manager = TxManager::new(pool.clone(), ids.clone());

    let roles: Arc<dyn RoleRepository> = Arc::new(SqlxRoleRepository::new());
    let role_ids: Arc<dyn IdGenerator> = Arc::new(
        sanvi_access::adapters::SystemRoleIdGenerator::new(ids.clone()),
    );
    let clock: Arc<dyn Clock> = Arc::new(sanvi_access::adapters::SystemClock);

    match kind {
        SeedKind::Roles => {
            let use_case = SeedRoles::new(tx_manager, roles, role_ids, clock);
            let changed = use_case.execute().await?;
            println!("seeded roles ({changed} created or updated)");
        }
        SeedKind::Access => {
            let store: Arc<dyn PermissionStore> = Arc::new(HttpKetoClient::new(
                config.ory.keto_read_url.clone(),
                config.ory.keto_write_url.clone(),
            )?);
            let memberships: Arc<dyn MembershipRepository> =
                Arc::new(SqlxMembershipRepository::new());
            let source = Arc::new(SqlxMembershipSource::new(tx_manager.clone(), memberships));
            let use_case = RebuildAccessTuples::new(tx_manager, roles, store, source);
            let tuples = use_case.execute().await?;
            println!("access tuples rebuilt ({tuples} tuples written)");
        }
        SeedKind::Features => {
            let use_case = SeedFeatures::new(
                tx_manager,
                Arc::new(sanvi_access::adapters::SqlxFeatureRepository::new()),
            );
            let created = use_case.execute().await?;
            println!("seeded features ({created} created)");
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct MembershipGrantArgs {
    pub email: String,
    pub role: String,
    pub tenant_slug: Option<String>,
    pub platform: bool,
}

pub async fn membership_grant(
    environment: &str,
    args: MembershipGrantArgs,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let config = load_config(environment)?;
    let pool = sanvi_persistence::build_pool(&config.database).await?;
    let tx_manager = TxManager::new(pool.clone(), ids.clone());
    let users: Arc<dyn UserRepository> = Arc::new(SqlxUserRepository::new());

    // The target user must already exist (registered via Kratos).
    let mut scope = tx_manager
        .begin_unscoped("cli_membership_grant", "platform-admin")
        .await?;
    let user = users
        .find_by_email(&mut scope, &Email::parse(&args.email)?)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no user with email {}", args.email))?;
    user.ensure_active()?;
    scope.commit().await?;

    let store: Arc<dyn PermissionStore> = Arc::new(HttpKetoClient::new(
        config.ory.keto_read_url.clone(),
        config.ory.keto_write_url.clone(),
    )?);

    if args.platform {
        // Platform assignments are Keto tuples (phase 03 gives them a
        // database home with audit).
        store
            .write(&[Tuple::role_membership(
                &format!("platform:{}", args.role),
                user.id(),
            )])
            .await?;
        println!(
            "granted platform role {} to {}",
            args.role,
            user.primary_email()
        );
        return Ok(());
    }

    let tenant_slug = args
        .tenant_slug
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--tenant <slug> is required without --platform"))?;
    let tenants = Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new());
    let mut scope = tx_manager
        .begin_unscoped("cli_membership_grant_tenant", "platform-admin")
        .await?;
    let tenant = tenants
        .find_by_slug(
            &mut scope,
            &sanvi_tenancy::domain::TenantSlug::parse(&tenant_slug)?,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("no tenant with slug {tenant_slug}"))?;
    scope.commit().await?;
    let tenant_id = tenant.id();

    let roles: Arc<dyn RoleRepository> = Arc::new(SqlxRoleRepository::new());
    let role_catalog: Arc<dyn RoleCatalog> = Arc::new(SqlxRoleCatalog::new(
        TxManager::new(pool.clone(), ids.clone()),
        roles.clone(),
    ));
    let memberships: Arc<dyn MembershipRepository> = Arc::new(SqlxMembershipRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new(ids));
    let clock: Arc<dyn sanvi_identity::domain::Clock> = Arc::new(SystemClock);

    let grant = GrantMembership::new(
        tx_manager.clone(),
        users,
        memberships.clone(),
        role_catalog,
        events,
        clock,
    );

    // Find the role id for the requested key in this tenant.
    let mut scope = tx_manager
        .begin_unscoped("cli_membership_role_lookup", "platform-admin")
        .await?;
    let assignable = roles.assignable_to(&mut scope, tenant_id).await?;
    scope.commit().await?;
    let role_id = assignable
        .iter()
        .find(|role| role.key() == args.role)
        .map(|role| role.id())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "role {} is not assignable in tenant {tenant_slug}",
                args.role
            )
        })?;

    grant
        .execute(
            tenant_id,
            GrantMembershipCommand {
                email: args.email.clone(),
                role_ids: vec![role_id],
            },
            user.id(),
        )
        .await?;

    // Sync Keto immediately (no worker required for CLI flows).
    let source = Arc::new(SqlxMembershipSource::new(tx_manager.clone(), memberships));
    let rebuild = RebuildAccessTuples::new(tx_manager, roles, store, source);
    let tuples = rebuild.execute().await?;
    println!(
        "granted role {} to {} in tenant {tenant_slug} ({tuples} tuples synced)",
        args.role, args.email
    );
    Ok(())
}
