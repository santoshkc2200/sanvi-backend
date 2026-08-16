//! The sqlx role repository (`access.roles` + `access.role_permissions`).
//!
//! Reads run under the audited `PlatformScope` (whose `sanvi.platform_scope`
//! GUC passes the RLS policy), which is how system templates and every
//! tenant's custom roles are all visible to the authorizer.

use std::collections::BTreeSet;

use async_trait::async_trait;
use sanvi_persistence::PlatformScope;
use sqlx::FromRow;
use uuid::Uuid;

use crate::domain::roles::{Role, RoleId, RoleScope};
use crate::domain::{AccessError, RoleRepository};

#[derive(Debug, Clone, FromRow)]
struct RoleRow {
    id: Uuid,
    scope: String,
    tenant_id: Option<Uuid>,
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
            role.id().as_uuid(),
            role.scope().as_str(),
            role.tenant_id(),
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
            role.id().as_uuid(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;

        for permission in role.permissions() {
            sqlx::query!(
                "INSERT INTO access.role_permissions (role_id, permission) VALUES ($1, $2)",
                role.id().as_uuid(),
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
        tenant_id: Option<Uuid>,
        key: &str,
    ) -> Result<Option<Role>, AccessError> {
        let row = sqlx::query_as!(
            RoleRow,
            "SELECT id, scope, tenant_id, key, name, is_system, created_at
             FROM access.roles
             WHERE scope = $1 AND tenant_id IS NOT DISTINCT FROM $2 AND key = $3",
            scope_kind.as_str(),
            tenant_id,
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
            id.as_uuid(),
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
            .bind(id.as_uuid())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn granting(
        &self,
        scope: &mut PlatformScope<'_>,
        permission: &str,
        tenant_id: Option<Uuid>,
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
            tenant_id,
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;

        let mut roles = Vec::with_capacity(rows.len());
        for row in rows {
            roles.push(self.hydrate(scope, row).await?);
        }
        Ok(roles)
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
        let mut roles = Vec::with_capacity(rows.len());
        for row in rows {
            roles.push(self.hydrate(scope, row).await?);
        }
        Ok(roles)
    }

    async fn assignable_to(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<Role>, AccessError> {
        let rows = sqlx::query_as!(
            RoleRow,
            "SELECT id, scope, tenant_id, key, name, is_system, created_at
             FROM access.roles
             WHERE (scope = 'tenant' AND tenant_id IS NULL AND is_system)
                OR (scope = 'tenant' AND tenant_id = $1)
             ORDER BY key",
            tenant_id,
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        let mut roles = Vec::with_capacity(rows.len());
        for row in rows {
            roles.push(self.hydrate(scope, row).await?);
        }
        Ok(roles)
    }
}

impl SqlxRoleRepository {
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
        Ok(Role::from_parts(
            RoleId::from_uuid(row.id),
            RoleScope::parse(&row.scope)?,
            row.tenant_id,
            row.key,
            row.name,
            permissions
                .into_iter()
                .map(|p| p.permission)
                .collect::<BTreeSet<_>>(),
            row.is_system,
            row.created_at,
        ))
    }
}
