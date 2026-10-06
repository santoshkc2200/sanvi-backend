//! The sqlx repositories for `platform_admin`: impersonation grants,
//! pending approvals, and the tenant admin read model (assembled over the
//! tenancy/identity/access ports under one audited transaction).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_access::domain::EntitlementRepository;
use sanvi_identity::domain::MembershipRepository;
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::PlatformScope;
use sanvi_tenancy::domain::{TenantRepository, TenantSearch, TenantSearchCursor, TenantStatus};
use sqlx::FromRow;

use crate::domain::PlatformAdminError;
use crate::domain::approvals::{ApprovalId, ApprovalStatus, PendingApproval};
use crate::domain::impersonation::{ImpersonationGrant, ImpersonationGrantId, ImpersonationMode};
use crate::domain::ports::{
    Clock, ImpersonationGrantRepository, PendingApprovalRepository, TenantAdminViewRepository,
};
use crate::domain::views::{TenantAdminView, TenantViewCursor, TenantViewSearch};

fn storage(err: impl std::fmt::Display) -> PlatformAdminError {
    PlatformAdminError::Storage(err.to_string())
}

const ANONYMISED_USER_ID: i64 = 1;

// ---------------------------------------------------------------------------
// Impersonation grants
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct GrantRow {
    id: i64,
    actor_id: i64,
    tenant_id: i64,
    target_user_id: i64,
    reason: String,
    mode: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by: Option<i64>,
}

fn to_grant(row: GrantRow) -> Result<ImpersonationGrant, PlatformAdminError> {
    Ok(ImpersonationGrant::from_parts(
        ImpersonationGrantId::try_from(row.id).map_err(storage)?,
        UserId::try_from(row.actor_id).map_err(storage)?,
        TenantId::try_from(row.tenant_id).map_err(storage)?,
        UserId::try_from(row.target_user_id).map_err(storage)?,
        row.reason,
        ImpersonationMode::parse(&row.mode)?,
        row.created_at,
        row.expires_at,
        row.revoked_at,
        row.revoked_by
            .map(UserId::try_from)
            .transpose()
            .map_err(storage)?,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxImpersonationGrantRepository;

impl SqlxImpersonationGrantRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ImpersonationGrantRepository for SqlxImpersonationGrantRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        grant: &ImpersonationGrant,
    ) -> Result<(), PlatformAdminError> {
        sqlx::query(
            "INSERT INTO platform_admin.impersonation_grants
               (id, actor_id, tenant_id, target_user_id, reason, mode,
                created_at, expires_at, revoked_at, revoked_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE SET
               revoked_at = EXCLUDED.revoked_at,
               revoked_by = EXCLUDED.revoked_by",
        )
        .bind(grant.id().as_i64())
        .bind(grant.actor_id().as_i64())
        .bind(grant.tenant_id().as_i64())
        .bind(grant.target_user_id().as_i64())
        .bind(grant.reason())
        .bind(grant.mode().as_str())
        .bind(grant.created_at())
        .bind(grant.expires_at())
        .bind(grant.revoked_at())
        .bind(grant.revoked_by().map(UserId::as_i64))
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ImpersonationGrantId,
    ) -> Result<Option<ImpersonationGrant>, PlatformAdminError> {
        let row: Option<GrantRow> = sqlx::query_as(
            "SELECT id, actor_id, tenant_id, target_user_id, reason, mode,
                    created_at, expires_at, revoked_at, revoked_by
             FROM platform_admin.impersonation_grants WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(to_grant).transpose()
    }

    async fn list_active(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Option<TenantId>,
        now: DateTime<Utc>,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<ImpersonationGrant>, PlatformAdminError> {
        let rows: Vec<GrantRow> = sqlx::query_as(
            "SELECT id, actor_id, tenant_id, target_user_id, reason, mode,
                    created_at, expires_at, revoked_at, revoked_by
             FROM platform_admin.impersonation_grants
             WHERE revoked_at IS NULL AND expires_at > $1
               AND ($2::bigint IS NULL OR tenant_id = $2)
               AND ($4::bigint IS NULL OR id < $4)
             ORDER BY id DESC LIMIT $3",
        )
        .bind(now)
        .bind(tenant_id.map(TenantId::as_i64))
        .bind(i64::from(limit))
        .bind(after_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(to_grant).collect()
    }

    async fn list_by_actor(
        &self,
        scope: &mut PlatformScope<'_>,
        actor_id: UserId,
    ) -> Result<Vec<ImpersonationGrant>, PlatformAdminError> {
        let rows: Vec<GrantRow> = sqlx::query_as(
            "SELECT id, actor_id, tenant_id, target_user_id, reason, mode,
                    created_at, expires_at, revoked_at, revoked_by
             FROM platform_admin.impersonation_grants WHERE actor_id = $1
             ORDER BY created_at DESC",
        )
        .bind(actor_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(to_grant).collect()
    }

    async fn list_by_target(
        &self,
        scope: &mut PlatformScope<'_>,
        target_user_id: UserId,
    ) -> Result<Vec<ImpersonationGrant>, PlatformAdminError> {
        let rows: Vec<GrantRow> = sqlx::query_as(
            "SELECT id, actor_id, tenant_id, target_user_id, reason, mode,
                    created_at, expires_at, revoked_at, revoked_by
             FROM platform_admin.impersonation_grants WHERE target_user_id = $1
             ORDER BY created_at DESC",
        )
        .bind(target_user_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(to_grant).collect()
    }

    async fn revoke_all_for_actor(
        &self,
        scope: &mut PlatformScope<'_>,
        actor_id: UserId,
    ) -> Result<usize, PlatformAdminError> {
        let result = sqlx::query(
            "UPDATE platform_admin.impersonation_grants
             SET revoked_at = now(), revoked_by = $1
             WHERE actor_id = $2 AND revoked_at IS NULL",
        )
        .bind(ANONYMISED_USER_ID)
        .bind(actor_id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(result.rows_affected() as usize)
    }

    async fn anonymise_target(
        &self,
        scope: &mut PlatformScope<'_>,
        target_user_id: UserId,
    ) -> Result<usize, PlatformAdminError> {
        let result = sqlx::query(
            "UPDATE platform_admin.impersonation_grants
             SET target_user_id = $1
             WHERE target_user_id = $2",
        )
        .bind(ANONYMISED_USER_ID)
        .bind(target_user_id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(result.rows_affected() as usize)
    }

    async fn count_active(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        now: DateTime<Utc>,
    ) -> Result<u32, PlatformAdminError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM platform_admin.impersonation_grants
             WHERE tenant_id = $1 AND revoked_at IS NULL AND expires_at > $2",
        )
        .bind(tenant_id.as_i64())
        .bind(now)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(count as u32)
    }

    async fn count_active_batch(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_ids: &[TenantId],
        now: DateTime<Utc>,
    ) -> Result<std::collections::HashMap<TenantId, u32>, PlatformAdminError> {
        let mut counts = std::collections::HashMap::new();
        if tenant_ids.is_empty() {
            return Ok(counts);
        }
        let ids: Vec<i64> = tenant_ids.iter().map(|id| id.as_i64()).collect();
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT tenant_id, count(*) FROM platform_admin.impersonation_grants
             WHERE tenant_id = ANY($1) AND revoked_at IS NULL AND expires_at > $2
             GROUP BY tenant_id",
        )
        .bind(&ids)
        .bind(now)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        for (tenant_id, count) in rows {
            counts.insert(
                TenantId::try_from(tenant_id).map_err(storage)?,
                count as u32,
            );
        }
        Ok(counts)
    }
}

// ---------------------------------------------------------------------------
// Pending approvals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
struct ApprovalRow {
    id: i64,
    requested_by: i64,
    action_type: String,
    payload: serde_json::Value,
    status: String,
    requested_at: DateTime<Utc>,
    decided_by: Option<i64>,
    decided_at: Option<DateTime<Utc>>,
}

fn to_approval(row: ApprovalRow) -> Result<PendingApproval, PlatformAdminError> {
    Ok(PendingApproval::from_parts(
        ApprovalId::try_from(row.id).map_err(storage)?,
        UserId::try_from(row.requested_by).map_err(storage)?,
        row.action_type,
        row.payload,
        ApprovalStatus::parse(&row.status)?,
        row.requested_at,
        row.decided_by
            .map(UserId::try_from)
            .transpose()
            .map_err(storage)?,
        row.decided_at,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxPendingApprovalRepository;

impl SqlxPendingApprovalRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PendingApprovalRepository for SqlxPendingApprovalRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        approval: &PendingApproval,
    ) -> Result<(), PlatformAdminError> {
        sqlx::query(
            "INSERT INTO platform_admin.pending_approvals
               (id, requested_by, action_type, payload, status, requested_at,
                decided_by, decided_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (id) DO UPDATE SET
               status = EXCLUDED.status,
               decided_by = EXCLUDED.decided_by,
               decided_at = EXCLUDED.decided_at",
        )
        .bind(approval.id().as_i64())
        .bind(approval.requested_by().as_i64())
        .bind(approval.action_type())
        .bind(approval.payload())
        .bind(approval.status().as_str())
        .bind(approval.requested_at())
        .bind(approval.decided_by().map(UserId::as_i64))
        .bind(approval.decided_at())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ApprovalId,
    ) -> Result<Option<PendingApproval>, PlatformAdminError> {
        let row: Option<ApprovalRow> = sqlx::query_as(
            "SELECT id, requested_by, action_type, payload, status, requested_at,
                    decided_by, decided_at
             FROM platform_admin.pending_approvals WHERE id = $1",
        )
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(to_approval).transpose()
    }

    async fn list_by_requester(
        &self,
        scope: &mut PlatformScope<'_>,
        requested_by: UserId,
    ) -> Result<Vec<PendingApproval>, PlatformAdminError> {
        let rows: Vec<ApprovalRow> = sqlx::query_as(
            "SELECT id, requested_by, action_type, payload, status, requested_at,
                    decided_by, decided_at
             FROM platform_admin.pending_approvals WHERE requested_by = $1
             ORDER BY requested_at DESC",
        )
        .bind(requested_by.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(to_approval).collect()
    }

    async fn anonymise_for_user(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<usize, PlatformAdminError> {
        let result = sqlx::query(
            "UPDATE platform_admin.pending_approvals
             SET requested_by = CASE WHEN requested_by = $1 THEN $2 ELSE requested_by END,
                 decided_by = CASE WHEN decided_by = $1 THEN $2 ELSE decided_by END
             WHERE requested_by = $1 OR decided_by = $1",
        )
        .bind(user_id.as_i64())
        .bind(ANONYMISED_USER_ID)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(result.rows_affected() as usize)
    }

    async fn list_pending(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<PendingApproval>, PlatformAdminError> {
        let rows: Vec<ApprovalRow> = sqlx::query_as(
            "SELECT id, requested_by, action_type, payload, status, requested_at,
                    decided_by, decided_at
             FROM platform_admin.pending_approvals
             WHERE status = 'pending'
               AND ($2::bigint IS NULL OR id > $2)
             ORDER BY id
             LIMIT $1",
        )
        .bind(i64::from(limit))
        .bind(after_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(to_approval).collect()
    }
}

// ---------------------------------------------------------------------------
// Tenant admin read model
// ---------------------------------------------------------------------------

/// The read model assembles tenancy basics (via tenancy's `TenantRepository`
/// search), member counts (identity's `MembershipRepository`), override
/// counts (access's `EntitlementRepository`) and active impersonations
/// (this context's grants) — over ports, one audited transaction, no
/// cross-schema SQL.
#[derive(Clone)]
pub struct SqlxTenantAdminViewRepository {
    tenants: std::sync::Arc<dyn TenantRepository>,
    memberships: std::sync::Arc<dyn MembershipRepository>,
    entitlements: std::sync::Arc<dyn EntitlementRepository>,
    grants: std::sync::Arc<dyn ImpersonationGrantRepository>,
    clock: std::sync::Arc<dyn Clock>,
}

impl SqlxTenantAdminViewRepository {
    pub fn new(
        tenants: std::sync::Arc<dyn TenantRepository>,
        memberships: std::sync::Arc<dyn MembershipRepository>,
        entitlements: std::sync::Arc<dyn EntitlementRepository>,
        grants: std::sync::Arc<dyn ImpersonationGrantRepository>,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Self {
        Self {
            tenants,
            memberships,
            entitlements,
            grants,
            clock,
        }
    }
}

#[async_trait]
impl TenantAdminViewRepository for SqlxTenantAdminViewRepository {
    async fn search(
        &self,
        scope: &mut PlatformScope<'_>,
        search: &TenantViewSearch,
    ) -> Result<(Vec<TenantAdminView>, Option<TenantViewCursor>), PlatformAdminError> {
        let status = match search.status.as_deref() {
            Some(raw) => Some(
                TenantStatus::parse(raw)
                    .map_err(|err| PlatformAdminError::InvalidSearch(err.to_string()))?,
            ),
            None => None,
        };
        let (tenants, next_cursor) = self
            .tenants
            .search(
                scope,
                &TenantSearch {
                    query: search.query.clone(),
                    status,
                    after: search.after.map(|cursor| TenantSearchCursor {
                        created_at: cursor.created_at,
                        id: cursor.tenant_id,
                    }),
                    limit: search
                        .limit
                        .unwrap_or(crate::application::tenant_views::DEFAULT_PAGE_LIMIT),
                },
            )
            .await
            .map_err(|err| PlatformAdminError::Storage(err.to_string()))?;
        let views = self.assemble(scope, &tenants).await?;
        let cursor = next_cursor.map(|cursor| TenantViewCursor {
            created_at: cursor.created_at,
            tenant_id: cursor.id,
        });
        Ok((views, cursor))
    }

    async fn get(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<TenantAdminView>, PlatformAdminError> {
        let Some(tenant) = self
            .tenants
            .find_by_id(scope, tenant_id)
            .await
            .map_err(|err| PlatformAdminError::Storage(err.to_string()))?
        else {
            return Ok(None);
        };
        let views = self.assemble(scope, &[tenant]).await?;
        Ok(views.into_iter().next())
    }
}

impl SqlxTenantAdminViewRepository {
    async fn assemble(
        &self,
        scope: &mut PlatformScope<'_>,
        tenants: &[sanvi_tenancy::domain::Tenant],
    ) -> Result<Vec<TenantAdminView>, PlatformAdminError> {
        // Three grouped counts for the whole page — the per-tenant form was
        // three queries per tenant row (TASK-021 11.2e).
        let tenant_ids: Vec<TenantId> = tenants.iter().map(|tenant| tenant.id()).collect();
        let member_counts = self
            .memberships
            .count_members_platform_batch(scope, &tenant_ids)
            .await
            .map_err(|err| PlatformAdminError::Storage(err.to_string()))?;
        let override_counts = self
            .entitlements
            .count_active_batch(scope, &tenant_ids, self.clock.now())
            .await
            .map_err(|err| PlatformAdminError::Storage(err.to_string()))?;
        let impersonation_counts = self
            .grants
            .count_active_batch(scope, &tenant_ids, self.clock.now())
            .await?;
        let mut views = Vec::with_capacity(tenants.len());
        for tenant in tenants {
            let tenant_id = tenant.id();
            let member_count = member_counts.get(&tenant_id).copied().unwrap_or(0);
            let override_count = override_counts.get(&tenant_id).copied().unwrap_or(0);
            let active_impersonations = impersonation_counts.get(&tenant_id).copied().unwrap_or(0);
            views.push(TenantAdminView {
                tenant_id,
                slug: tenant.slug().to_string(),
                display_name: tenant.display_name().to_string(),
                status: tenant.status().as_str().to_string(),
                region: tenant.region().to_string(),
                default_locale: tenant.default_locale().to_string(),
                created_at: tenant.created_at(),
                member_count,
                override_count,
                active_impersonations,
            });
        }
        Ok(views)
    }
}

// ---------------------------------------------------------------------------
// Application-fee configuration (09.6). The platform's own fee is data:
// disable per tenant with no deploy. Every change writes an audit entry
// (actor + reason) in the SAME transaction as the change — a fee change
// without its record never commits.
// ---------------------------------------------------------------------------

use crate::domain::fees::ApplicationFeeConfig;
use crate::domain::ports::ApplicationFeeAdmin;

#[derive(Debug, FromRow)]
struct FeeRow {
    tenant_id: i64,
    basis_points: Option<i32>,
    fixed_amount_minor: Option<i64>,
    fixed_currency: Option<String>,
    minimum_minor: i64,
    enabled: bool,
}

impl TryFrom<FeeRow> for ApplicationFeeConfig {
    type Error = PlatformAdminError;

    fn try_from(row: FeeRow) -> Result<Self, Self::Error> {
        Ok(Self {
            tenant_id: TenantId::try_from(row.tenant_id).map_err(storage)?,
            basis_points: row
                .basis_points
                .map(|v| u32::try_from(v.max(0) as i64).unwrap_or(0)),
            fixed_amount_minor: row.fixed_amount_minor,
            fixed_currency: row.fixed_currency,
            minimum_minor: row.minimum_minor,
            enabled: row.enabled,
        })
    }
}

fn fee_json(config: &ApplicationFeeConfig) -> serde_json::Value {
    serde_json::json!({
        "basis_points": config.basis_points,
        "fixed_amount_minor": config.fixed_amount_minor,
        "fixed_currency": config.fixed_currency,
        "minimum_minor": config.minimum_minor,
        "enabled": config.enabled,
    })
}

/// The `platform_admin.application_fees` repository. Owns its
/// transactions (the operator handlers have no scope of their own, the
/// same way the entitlement-admin adapter runs its own use cases).
pub struct SqlxFeeRepository {
    tx_manager: sanvi_persistence::TxManager,
    audit: std::sync::Arc<dyn sanvi_audit::domain::AuditWriter>,
}

impl SqlxFeeRepository {
    pub fn new(
        tx_manager: sanvi_persistence::TxManager,
        audit: std::sync::Arc<dyn sanvi_audit::domain::AuditWriter>,
    ) -> Self {
        Self { tx_manager, audit }
    }

    async fn read(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<ApplicationFeeConfig>, PlatformAdminError> {
        let row = sqlx::query_as::<_, FeeRow>(
            "SELECT tenant_id, basis_points, fixed_amount_minor, fixed_currency,
                    minimum_minor, enabled
             FROM platform_admin.application_fees
             WHERE tenant_id = $1",
        )
        .bind(tenant_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryFrom::try_from).transpose()
    }

    async fn write(
        &self,
        config: ApplicationFeeConfig,
        actor_id: Option<UserId>,
        reason: Option<String>,
        action: &str,
        before: Option<ApplicationFeeConfig>,
        after_enabled_only: bool,
    ) -> Result<(), PlatformAdminError> {
        config.validate()?;
        let mut scope = self
            .tx_manager
            .begin_unscoped("application_fee_write", "platform_admin")
            .await
            .map_err(storage)?;
        match action {
            "platform_admin.application_fee.set" => {
                sqlx::query(
                    "INSERT INTO platform_admin.application_fees
                       (tenant_id, basis_points, fixed_amount_minor, fixed_currency,
                        minimum_minor, enabled, set_by, reason)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (tenant_id) DO UPDATE SET
                       basis_points = EXCLUDED.basis_points,
                       fixed_amount_minor = EXCLUDED.fixed_amount_minor,
                       fixed_currency = EXCLUDED.fixed_currency,
                       minimum_minor = EXCLUDED.minimum_minor,
                       enabled = EXCLUDED.enabled,
                       set_by = EXCLUDED.set_by,
                       reason = EXCLUDED.reason,
                       updated_at = now()",
                )
                .bind(config.tenant_id.as_i64())
                .bind(config.basis_points.map(|v| v as i32))
                .bind(config.fixed_amount_minor)
                .bind(&config.fixed_currency)
                .bind(config.minimum_minor)
                .bind(config.enabled)
                .bind(actor_id.map(UserId::as_i64))
                .bind(&reason)
                .execute(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(storage)?;
            }
            _ => {
                sqlx::query(
                    "UPDATE platform_admin.application_fees
                     SET enabled = false, set_by = $2, reason = $3, updated_at = now()
                     WHERE tenant_id = $1",
                )
                .bind(config.tenant_id.as_i64())
                .bind(actor_id.map(UserId::as_i64))
                .bind(&reason)
                .execute(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(storage)?;
            }
        }
        let after = if after_enabled_only {
            serde_json::json!({ "enabled": false })
        } else {
            fee_json(&config)
        };
        self.audit
            .append(
                &mut scope,
                &sanvi_audit::domain::AuditEntryData {
                    occurred_at: chrono::Utc::now(),
                    actor_type: sanvi_audit::domain::ActorType::PlatformAdmin,
                    actor_id,
                    on_behalf_of: None,
                    tenant_id: Some(config.tenant_id),
                    action: action.to_string(),
                    resource_type: Some("application_fee".to_string()),
                    resource_id: Some(config.tenant_id.to_string()),
                    before: before.as_ref().map(fee_json),
                    after: Some(after),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(storage)?;
        scope.commit().await.map_err(storage)?;
        Ok(())
    }
}

#[async_trait]
impl ApplicationFeeAdmin for SqlxFeeRepository {
    async fn get(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<ApplicationFeeConfig>, PlatformAdminError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("application_fee_read", "platform_admin")
            .await
            .map_err(storage)?;
        let config = self.read(&mut scope, tenant_id).await;
        let _ = scope.rollback().await;
        config
    }

    async fn set(
        &self,
        config: ApplicationFeeConfig,
        actor_id: Option<UserId>,
        reason: Option<String>,
    ) -> Result<(), PlatformAdminError> {
        // Read the previous value first (best-effort: an unreadable
        // `before` must not block a corrective change).
        let before = self.get(config.tenant_id).await.ok().flatten();
        self.write(
            config,
            actor_id,
            reason,
            "platform_admin.application_fee.set",
            before,
            false,
        )
        .await
    }

    async fn disable(
        &self,
        tenant_id: TenantId,
        actor_id: Option<UserId>,
        reason: Option<String>,
    ) -> Result<(), PlatformAdminError> {
        let before = self.get(tenant_id).await.ok().flatten();
        let placeholder = ApplicationFeeConfig {
            tenant_id,
            ..before
                .clone()
                .unwrap_or(crate::domain::fees::ApplicationFeeConfig {
                    tenant_id,
                    basis_points: None,
                    fixed_amount_minor: None,
                    fixed_currency: None,
                    minimum_minor: 0,
                    enabled: false,
                })
        };
        self.write(
            placeholder,
            actor_id,
            reason,
            "platform_admin.application_fee.disable",
            before,
            true,
        )
        .await
    }
}
