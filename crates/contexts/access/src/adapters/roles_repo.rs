//! The sqlx role repository (`access.roles` + `access.role_permissions`).
//!
//! Reads run under the audited `PlatformScope` (whose `sanvi.platform_scope`
//! GUC passes the RLS policy), which is how system templates and every
//! tenant's custom roles are all visible to the authorizer.

use std::collections::BTreeSet;

use async_trait::async_trait;
use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::PlatformScope;
use sqlx::FromRow;

use crate::domain::roles::{Role, RoleScope};
use crate::domain::{AccessError, RoleRepository};

#[derive(Debug, Clone, FromRow)]
struct RoleRow {
    id: i64,
    scope: String,
    tenant_id: Option<i64>,
    key: String,
    name: String,
    is_system: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, FromRow)]
struct PermissionRow {
    permission: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxRoleRepository;

impl SqlxRoleRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RoleRepository for SqlxRoleRepository {
    async fn save(&self, scope: &mut PlatformScope<'_>, role: &Role) -> Result<(), AccessError> {
        sqlx::query!(
            "INSERT INTO access.roles (id, scope, tenant_id, key, name, is_system, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO UPDATE SET
               scope = EXCLUDED.scope,
               tenant_id = EXCLUDED.tenant_id,
               key = EXCLUDED.key,
               name = EXCLUDED.name,
               is_system = EXCLUDED.is_system",
            role.id().as_i64(),
            role.scope().as_str(),
            role.tenant_id().map(|id| id.as_i64()),
            role.key(),
            role.name(),
            role.is_system(),
            role.created_at(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;

        sqlx::query!(
            "DELETE FROM access.role_permissions WHERE role_id = $1",
            role.id().as_i64(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;

        for permission in role.permissions() {
            sqlx::query!(
                "INSERT INTO access.role_permissions (role_id, permission) VALUES ($1, $2)",
                role.id().as_i64(),
                permission,
            )
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        }
        Ok(())
    }

    async fn find_by_key(
        &self,
        scope: &mut PlatformScope<'_>,
        scope_kind: RoleScope,
        tenant_id: Option<TenantId>,
        key: &str,
    ) -> Result<Option<Role>, AccessError> {
        let row = sqlx::query_as!(
            RoleRow,
            "SELECT id, scope, tenant_id, key, name, is_system, created_at
             FROM access.roles
             WHERE scope = $1 AND tenant_id IS NOT DISTINCT FROM $2 AND key = $3",
            scope_kind.as_str(),
            tenant_id.map(|id| id.as_i64()),
            key,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        let Some(row) = row else {
            return Ok(None);
        };
        self.hydrate(scope, row).await.map(Some)
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: RoleId,
    ) -> Result<Option<Role>, AccessError> {
        let row = sqlx::query_as!(
            RoleRow,
            "SELECT id, scope, tenant_id, key, name, is_system, created_at
             FROM access.roles WHERE id = $1",
            id.as_i64(),
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        let Some(row) = row else {
            return Ok(None);
        };
        self.hydrate(scope, row).await.map(Some)
    }

    async fn delete(&self, scope: &mut PlatformScope<'_>, id: RoleId) -> Result<(), AccessError> {
        sqlx::query("DELETE FROM access.roles WHERE id = $1")
            .bind(id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn granting(
        &self,
        scope: &mut PlatformScope<'_>,
        permission: &str,
        tenant_id: Option<TenantId>,
    ) -> Result<Vec<Role>, AccessError> {
        let rows = sqlx::query_as!(
            RoleRow,
            "SELECT DISTINCT r.id, r.scope, r.tenant_id, r.key, r.name, r.is_system, r.created_at
             FROM access.roles r
             JOIN access.role_permissions p ON p.role_id = r.id
             WHERE p.permission = $1
               AND (
                 -- platform roles apply everywhere
                 r.scope = 'platform'
                 -- system tenant templates apply to every tenant
                 OR (r.scope = 'tenant' AND r.tenant_id IS NULL AND r.is_system)
                 -- custom roles apply to their own tenant only
                 OR (r.scope = 'tenant' AND r.tenant_id IS NOT NULL AND r.tenant_id = $2)
               )",
            permission,
            tenant_id.map(|id| id.as_i64()),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;

        self.hydrate_many(scope, rows).await
    }

    async fn list_all(&self, scope: &mut PlatformScope<'_>) -> Result<Vec<Role>, AccessError> {
        let rows = sqlx::query_as!(
            RoleRow,
            "SELECT id, scope, tenant_id, key, name, is_system, created_at
             FROM access.roles ORDER BY scope, key",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        self.hydrate_many(scope, rows).await
    }

    async fn assignable_to(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Role>, AccessError> {
        let rows = sqlx::query_as!(
            RoleRow,
            "SELECT id, scope, tenant_id, key, name, is_system, created_at
             FROM access.roles
             WHERE (scope = 'tenant' AND tenant_id IS NULL AND is_system)
                OR (scope = 'tenant' AND tenant_id = $1)
             ORDER BY key",
            tenant_id.as_i64(),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        self.hydrate_many(scope, rows).await
    }
}

impl SqlxRoleRepository {
    /// Hydrate one role (single-row read paths).
    async fn hydrate(
        &self,
        scope: &mut PlatformScope<'_>,
        row: RoleRow,
    ) -> Result<Role, AccessError> {
        let permissions = sqlx::query_as!(
            PermissionRow,
            "SELECT permission FROM access.role_permissions WHERE role_id = $1 ORDER BY permission",
            row.id,
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        let mut permissions_by_role = std::collections::BTreeMap::new();
        permissions_by_role.insert(
            row.id,
            permissions
                .into_iter()
                .map(|p| p.permission)
                .collect::<BTreeSet<_>>(),
        );
        self.build_role(row, &permissions_by_role)
    }

    /// Hydrate a page of roles with **one** permission query for the whole
    /// batch — per-row `SELECT … WHERE role_id = $1` here was the N+1 that
    /// compounded into every role listing and, through `SqlxRoleCatalog`,
    /// into `/me` (TASK-021 11.2e).
    async fn hydrate_many(
        &self,
        scope: &mut PlatformScope<'_>,
        rows: Vec<RoleRow>,
    ) -> Result<Vec<Role>, AccessError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
        let links: Vec<(i64, String)> = sqlx::query_as(
            "SELECT role_id, permission
             FROM access.role_permissions
             WHERE role_id = ANY($1)
             ORDER BY role_id, permission",
        )
        .bind(&ids)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        let mut permissions_by_role: std::collections::BTreeMap<i64, BTreeSet<String>> =
            std::collections::BTreeMap::new();
        for (role_id, permission) in links {
            permissions_by_role
                .entry(role_id)
                .or_default()
                .insert(permission);
        }
        rows.into_iter()
            .map(|row| self.build_role(row, &permissions_by_role))
            .collect()
    }

    fn build_role(
        &self,
        row: RoleRow,
        permissions_by_role: &std::collections::BTreeMap<i64, BTreeSet<String>>,
    ) -> Result<Role, AccessError> {
        let permissions = permissions_by_role
            .get(&row.id)
            .cloned()
            .unwrap_or_default();
        let tenant_id = row
            .tenant_id
            .map(TenantId::try_from)
            .transpose()
            .map_err(|err| AccessError::Storage(format!("invalid tenant id: {err}")))?;
        Ok(Role::from_parts(
            RoleId::try_from(row.id)
                .map_err(|err| AccessError::Storage(format!("invalid role id: {err}")))?,
            RoleScope::parse(&row.scope)?,
            tenant_id,
            row.key,
            row.name,
            permissions,
            row.is_system,
            row.created_at,
        ))
    }
}
