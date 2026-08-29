//! Phase-03 operator commands: feature catalog, entitlement overrides,
//! role listing, impersonation review, audit verification, and four-eyes
//! approvals. All commands run under the audited platform scope with the
//! `platform-admin` CLI actor (the API's permission checks do not apply to
//! a local operator session; every mutation is still audited).

use std::sync::Arc;

use crate::config::load as load_config;
use sanvi_access::{
    adapters::{SqlxEntitlementRepository, SqlxFeatureRepository, SqlxRoleRepository},
    application::{
        DefineFeature, DefineFeatureCommand, GrantEntitlement, GrantEntitlementCommand,
        GrantEntitlementOutput, ListFeatures, ListRoles, ListTenantOverrides, RevokeEntitlement,
        RevokeEntitlementCommand,
    },
    domain::{
        ApprovalGate, EntitlementCache, EntitlementRepository, EventPublisher, FeatureKey,
        FeatureKind, FeatureRepository, Visibility,
    },
};
use sanvi_audit::{
    adapters::{SqlxAuditReader, SqlxAuditWriter},
    application::{ListAuditLog, VerifyAuditChain},
};
use sanvi_kernel::{RawIdGenerator, TenantId, UserId};
use sanvi_persistence::TxManager;
use sanvi_platform_admin::{
    adapters::{
        ApprovalGateAdapter, SqlxImpersonationGrantRepository, SqlxPendingApprovalRepository,
        SystemClock, SystemIdGenerator,
    },
    application::approvals::{DecideApproval, RecordApprovalRequest},
    application::{
        ApproveRequest, DecideApprovalCommand, ListImpersonations, ListPendingApprovals,
        RejectRequest, RevokeImpersonation,
    },
    domain::EntitlementAdmin,
};
use sanvi_tenancy::{
    adapters::repo::SqlxTenantRepository,
    domain::{TenantRepository, TenantSlug},
};

/// The CLI's recorded actor (audit entries); a local operator session.
fn cli_actor() -> UserId {
    UserId::try_from(1).expect("the reserved CLI actor ID is valid")
}

async fn connect(environment: &str) -> anyhow::Result<sanvi_persistence::PgPool> {
    let config = load_config(environment)?;
    sanvi_persistence::build_pool(&config.database).await
}

async fn tenant_id_by_slug(
    environment: &str,
    slug: &str,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<TenantId> {
    let pool = connect(environment).await?;
    let tx_manager = TxManager::new(pool, ids);
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let mut scope = tx_manager
        .begin_unscoped("cli_tenant_lookup", "platform-admin")
        .await?;
    let tenant = tenants
        .find_by_slug(&mut scope, &TenantSlug::parse(slug)?)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no tenant with slug {slug}"))?;
    scope.commit().await?;
    Ok(tenant.id())
}

// ---------------------------------------------------------------------------
// Features
// ---------------------------------------------------------------------------

pub async fn feature_list(environment: &str, ids: Arc<dyn RawIdGenerator>) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let use_case = ListFeatures::new(
        TxManager::new(pool, ids),
        Arc::new(SqlxFeatureRepository::new()),
    );
    let features = use_case.execute().await?;
    for feature in &features {
        let deprecated = feature
            .deprecated_at()
            .map(|at| format!(" (deprecated {at})"))
            .unwrap_or_default();
        println!(
            "{}\t{}\t{}\tdefault_enabled={}\tdefault_limit={}{}",
            feature.key(),
            feature.name(),
            feature.kind().as_str(),
            feature.default_enabled(),
            feature
                .default_limit()
                .map(|limit| limit.to_string())
                .unwrap_or_else(|| "-".to_string()),
            deprecated,
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct FeatureDefineArgs<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub kind: &'a str,
    pub default_enabled: bool,
    pub default_limit: Option<i64>,
    pub visibility: &'a str,
}

pub async fn feature_define(
    environment: &str,
    args: FeatureDefineArgs<'_>,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let use_case = DefineFeature::new(
        TxManager::new(pool, ids.clone()),
        Arc::new(SqlxFeatureRepository::new()),
        Arc::new(sanvi_access::adapters::OutboxEventPublisher::new(
            ids.clone(),
        )),
        Arc::new(sanvi_access::adapters::SystemClock),
        Arc::new(SqlxAuditWriter::new(ids)),
    );
    let feature = use_case
        .execute(
            cli_actor(),
            None,
            DefineFeatureCommand {
                key: FeatureKey::parse(args.key)?,
                name: args.name.to_string(),
                kind: FeatureKind::parse(args.kind)?,
                default_enabled: args.default_enabled,
                default_limit: args.default_limit,
                visibility: Visibility::parse(args.visibility)?,
            },
        )
        .await?;
    println!("feature {} defined", feature.key());
    Ok(())
}

// ---------------------------------------------------------------------------
// Entitlements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EntitlementGrantArgs {
    pub tenant_slug: String,
    pub feature: String,
    pub enabled: bool,
    pub limit: Option<i64>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reason: Option<String>,
}

pub async fn entitlement_grant(
    environment: &str,
    args: EntitlementGrantArgs,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let tenant_id = tenant_id_by_slug(environment, &args.tenant_slug, ids.clone()).await?;
    let config = load_config(environment)?;
    let pool = connect(environment).await?;
    let tx_manager = TxManager::new(pool, ids.clone());
    let record_approval = Arc::new(RecordApprovalRequest::new(
        Arc::new(SqlxPendingApprovalRepository::new()),
        Arc::new(SystemIdGenerator::new(ids.clone())),
        Arc::new(SystemClock),
    ));
    let use_case = GrantEntitlement::new(
        tx_manager,
        Arc::new(SqlxFeatureRepository::new()),
        Arc::new(SqlxEntitlementRepository::new()),
        Arc::new(sanvi_access::adapters::OutboxEventPublisher::new(
            ids.clone(),
        )),
        Arc::new(sanvi_access::adapters::SystemClock),
        Arc::new(SqlxAuditWriter::new(ids)),
        Arc::new(ApprovalGateAdapter::new(record_approval)),
        sanvi_access::application::entitlements::EntitlementPolicy {
            approval_threshold: config.access.entitlement_approval_threshold,
        },
    );
    match use_case
        .execute(
            cli_actor(),
            None,
            GrantEntitlementCommand {
                tenant_id,
                feature_key: FeatureKey::parse(&args.feature)?,
                enabled: args.enabled,
                limit: args.limit,
                expires_at: args.expires_at,
                reason: args.reason,
                skip_approval: false,
            },
        )
        .await?
    {
        GrantEntitlementOutput::Granted => println!("override granted"),
        GrantEntitlementOutput::ApprovalRequired { approval_id } => println!(
            "override requires four-eyes approval (approval id: {approval_id}); \
             run `sanvi approval approve --id {approval_id}` as another operator"
        ),
    }
    Ok(())
}

pub async fn entitlement_revoke(
    environment: &str,
    tenant_slug: &str,
    feature: &str,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let tenant_id = tenant_id_by_slug(environment, tenant_slug, ids.clone()).await?;
    let pool = connect(environment).await?;
    let use_case = RevokeEntitlement::new(
        TxManager::new(pool, ids.clone()),
        Arc::new(SqlxEntitlementRepository::new()),
        Arc::new(sanvi_access::adapters::NoopEntitlementCache),
        Arc::new(sanvi_access::adapters::OutboxEventPublisher::new(
            ids.clone(),
        )),
        Arc::new(sanvi_access::adapters::SystemClock),
        Arc::new(SqlxAuditWriter::new(ids)),
    );
    use_case
        .execute(
            cli_actor(),
            None,
            RevokeEntitlementCommand {
                tenant_id,
                feature_key: FeatureKey::parse(feature)?,
            },
        )
        .await?;
    println!("override revoked");
    Ok(())
}

pub async fn entitlement_list(
    environment: &str,
    tenant_slug: &str,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let tenant_id = tenant_id_by_slug(environment, tenant_slug, ids.clone()).await?;
    let pool = connect(environment).await?;
    let use_case = ListTenantOverrides::new(
        TxManager::new(pool, ids),
        Arc::new(SqlxEntitlementRepository::new()),
    );
    let rows = use_case.execute(tenant_id).await?;
    for row in &rows {
        println!(
            "{}\t{}\tenabled={}\tlimit={}\texpires={}\tgranted_by={}\treason={}",
            row.feature(),
            row.source().as_str(),
            row.enabled(),
            row.limit()
                .map(|limit| limit.to_string())
                .unwrap_or_else(|| "-".to_string()),
            row.expires_at()
                .map(|at| at.to_rfc3339())
                .unwrap_or_else(|| "-".to_string()),
            row.granted_by()
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            row.granted_reason().unwrap_or("-"),
        );
    }
    if rows.is_empty() {
        println!("(no override rows)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

pub async fn role_list(environment: &str, ids: Arc<dyn RawIdGenerator>) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let use_case = ListRoles::new(
        TxManager::new(pool, ids),
        Arc::new(SqlxRoleRepository::new()),
    );
    let roles = use_case.execute_platform().await?;
    for role in &roles {
        println!(
            "{}\t{}\t{}\tpermissions={}\tsystem={}",
            role.scope().as_str(),
            role.key(),
            role.name(),
            role.permissions().len(),
            role.is_system(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Impersonation review
// ---------------------------------------------------------------------------

pub async fn impersonation_list(
    environment: &str,
    tenant_slug: Option<&str>,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let tenant_id = match tenant_slug {
        Some(slug) => Some(tenant_id_by_slug(environment, slug, ids.clone()).await?),
        None => None,
    };
    let pool = connect(environment).await?;
    let use_case = ListImpersonations::new(
        TxManager::new(pool, ids),
        Arc::new(SqlxImpersonationGrantRepository::new()),
        Arc::new(SystemClock),
    );
    let grants = use_case.execute(tenant_id, 100).await?;
    for grant in &grants {
        println!(
            "{}: actor={} target={} tenant={} mode={} expires={} reason={}",
            grant.id,
            grant.actor_id,
            grant.target_user_id,
            grant.tenant_id,
            grant.mode.as_str(),
            grant.expires_at.to_rfc3339(),
            grant.reason,
        );
    }
    if grants.is_empty() {
        println!("(no active grants)");
    }
    Ok(())
}

pub async fn impersonation_revoke(
    environment: &str,
    grant_id: sanvi_platform_admin::domain::ImpersonationGrantId,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let use_case = RevokeImpersonation::new(
        TxManager::new(pool, ids.clone()),
        Arc::new(SqlxImpersonationGrantRepository::new()),
        Arc::new(sanvi_platform_admin::adapters::OutboxEventPublisher::new(
            ids.clone(),
        )),
        Arc::new(SystemClock),
        Arc::new(SqlxAuditWriter::new(ids)),
    );
    use_case.execute(cli_actor(), None, grant_id).await?;
    println!("grant {grant_id} revoked");
    Ok(())
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

pub async fn audit_verify(environment: &str, ids: Arc<dyn RawIdGenerator>) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let use_case = VerifyAuditChain::new(
        TxManager::new(pool, ids),
        Arc::new(SqlxAuditReader::new()),
        1000,
    );
    let report = use_case.execute().await?;
    if report.verified {
        println!("audit chain verified ({} entries)", report.entries_checked);
        Ok(())
    } else {
        anyhow::bail!(
            "AUDIT CHAIN VERIFICATION FAILED: {}",
            report.first_mismatch.unwrap_or_default()
        )
    }
}

pub async fn audit_list(
    environment: &str,
    tenant_id: Option<TenantId>,
    action: Option<String>,
    limit: u32,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let use_case = ListAuditLog::new(TxManager::new(pool, ids), Arc::new(SqlxAuditReader::new()));
    let page = use_case
        .execute(
            sanvi_audit::domain::AuditFilters {
                tenant_id,
                actor_id: None,
                action,
                since: None,
                until: None,
            },
            None,
            limit,
        )
        .await?;
    for entry in &page.entries {
        println!(
            "{}\t{}\tactor={}\ttenant={}\t{}\t{}/{}",
            entry.seq,
            entry.occurred_at.to_rfc3339(),
            entry
                .actor_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| format!("<{}>", entry.actor_type.as_str())),
            entry
                .tenant_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            entry.action,
            entry.resource_type.as_deref().unwrap_or("-"),
            entry.resource_id.as_deref().unwrap_or("-"),
        );
    }
    if page.entries.is_empty() {
        println!("(no entries)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Four-eyes approvals
// ---------------------------------------------------------------------------

pub async fn approval_list(environment: &str, ids: Arc<dyn RawIdGenerator>) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let use_case = ListPendingApprovals::new(
        TxManager::new(pool, ids),
        Arc::new(SqlxPendingApprovalRepository::new()),
    );
    let approvals = use_case.execute(100).await?;
    for approval in &approvals {
        println!(
            "{}\t{}\trequested_by={}\tpayload={}",
            approval.id, approval.action_type, approval.requested_by, approval.payload,
        );
    }
    if approvals.is_empty() {
        println!("(no pending approvals)");
    }
    Ok(())
}

pub async fn approval_decide(
    environment: &str,
    approval_id: sanvi_platform_admin::domain::ApprovalId,
    approve: bool,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let tx_manager = TxManager::new(pool, ids.clone());

    // The approval executor re-runs the access grant with four-eyes
    // skipped (the approval is the second pair of eyes).
    let entitlement_admin: Arc<dyn EntitlementAdmin> =
        Arc::new(control_entitlement_admin(tx_manager.clone(), ids.clone()));
    let decide = Arc::new(DecideApproval::new(
        tx_manager,
        Arc::new(SqlxPendingApprovalRepository::new()),
        entitlement_admin,
        Arc::new(SystemClock),
        Arc::new(SqlxAuditWriter::new(ids)),
    ));
    let outcome = if approve {
        ApproveRequest::new(decide)
            .execute(
                cli_actor(),
                None,
                DecideApprovalCommand {
                    approval_id,
                    approve: true,
                },
            )
            .await?
    } else {
        RejectRequest::new(decide)
            .execute(
                cli_actor(),
                None,
                DecideApprovalCommand {
                    approval_id,
                    approve: false,
                },
            )
            .await?
    };
    println!("approval {}: {}", approval_id, outcome.status.as_str());
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared wiring helpers
// ---------------------------------------------------------------------------

fn noop_approval_gate() -> Arc<dyn ApprovalGate> {
    Arc::new(sanvi_platform_admin::adapters::NoopApprovalGate)
}

/// The `EntitlementAdmin` adapter for the CLI's approval executor (reuses
/// the composition root's adapter from `sanvi-api`).
fn control_entitlement_admin(
    tx_manager: TxManager,
    ids: Arc<dyn RawIdGenerator>,
) -> sanvi_api::AccessEntitlementAdmin {
    let features: Arc<dyn FeatureRepository> = Arc::new(SqlxFeatureRepository::new());
    let entitlements: Arc<dyn EntitlementRepository> = Arc::new(SqlxEntitlementRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(
        sanvi_access::adapters::OutboxEventPublisher::new(ids.clone()),
    );
    let audit = Arc::new(SqlxAuditWriter::new(ids));
    let cache: Arc<dyn EntitlementCache> = Arc::new(sanvi_access::adapters::NoopEntitlementCache);

    let grant = Arc::new(GrantEntitlement::new(
        tx_manager.clone(),
        features,
        entitlements.clone(),
        events.clone(),
        Arc::new(sanvi_access::adapters::SystemClock),
        audit.clone(),
        noop_approval_gate(),
        sanvi_access::application::entitlements::EntitlementPolicy::default(),
    ));
    let revoke = Arc::new(RevokeEntitlement::new(
        tx_manager.clone(),
        entitlements.clone(),
        cache,
        events,
        Arc::new(sanvi_access::adapters::SystemClock),
        audit,
    ));
    let list = Arc::new(ListTenantOverrides::new(tx_manager, entitlements));
    sanvi_api::AccessEntitlementAdmin::new(grant, revoke, list)
}
