//! Role authoring: create, update and delete *custom tenant roles*, with
//! the no-escalation invariant: a role may only grant permissions the
//! granting actor already holds in that tenant. System roles are immutable;
//! platform roles are never authorable here (they are seeded).

use std::collections::BTreeSet;
use std::sync::Arc;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::application::storage;
use crate::domain::events::AccessEvent;
use crate::domain::permissions::{Permission, PermissionScope, lookup};
use crate::domain::ports::{Authorizer, Decision, EventPublisher, ResourceRef};
use crate::domain::roles::{Role, RoleId, RoleScope};
use crate::domain::{AccessError, Clock, IdGenerator, RoleRepository};

/// Validates and normalizes a permission set: every key must be declared
/// and tenant-assignable.
fn normalize_permissions(raw: &[String]) -> Result<BTreeSet<String>, AccessError> {
    let mut permissions = BTreeSet::new();
    for key in raw {
        let permission = Permission::parse(key)?;
        let def = lookup(permission.as_str())
            .ok_or_else(|| AccessError::UnknownPermission(permission.as_str().to_string()))?;
        if def.scope != PermissionScope::Tenant {
            return Err(AccessError::PermissionScopeMismatch {
                permission: permission.as_str().to_string(),
                scope: def.scope.as_str().to_string(),
            });
        }
        permissions.insert(permission.as_str().to_string());
    }
    Ok(permissions)
}

/// The no-escalation check: the actor must hold every permission the new
/// role grants, in the tenant the role belongs to. Fail closed — a backend
/// error during the check blocks the grant rather than allowing it.
async fn ensure_actor_holds(
    authorizer: &Arc<dyn Authorizer>,
    actor: Uuid,
    tenant_id: Uuid,
    permissions: &BTreeSet<String>,
) -> Result<(), AccessError> {
    for permission in permissions {
        let decision = authorizer
            .check(
                actor,
                &Permission::parse(permission)?,
                &ResourceRef::Tenant(tenant_id),
            )
            .await?;
        if decision != Decision::Allowed {
            return Err(AccessError::Escalation {
                permission: permission.clone(),
            });
        }
    }
    Ok(())
}

pub struct CreateCustomRoleCommand {
    pub tenant_id: Uuid,
    pub key: String,
    pub name: String,
    pub permissions: Vec<String>,
}

pub struct CreateCustomRole {
    tx_manager: TxManager,
    roles: Arc<dyn RoleRepository>,
    authorizer: Arc<dyn Authorizer>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl CreateCustomRole {
    pub fn new(
        tx_manager: TxManager,
        roles: Arc<dyn RoleRepository>,
        authorizer: Arc<dyn Authorizer>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            roles,
            authorizer,
            events,
            ids,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        on_behalf_of: Option<Uuid>,
        cmd: CreateCustomRoleCommand,
    ) -> Result<Role, AccessError> {
        let permissions = normalize_permissions(&cmd.permissions)?;
        ensure_actor_holds(&self.authorizer, actor, cmd.tenant_id, &permissions).await?;

        let role = Role::new(
            self.ids.next_role_id(),
            RoleScope::Tenant,
            Some(cmd.tenant_id),
            cmd.key.clone(),
            cmd.name.clone(),
            permissions.clone(),
            false,
            self.clock.now(),
        )?;

        let mut scope = self
            .tx_manager
            .begin_unscoped("create_role", "tenant-admin")
            .await
            .map_err(storage)?;
        if self
            .roles
            .find_by_key(
                &mut scope,
                RoleScope::Tenant,
                Some(cmd.tenant_id),
                role.key(),
            )
            .await?
            .is_some()
        {
            return Err(AccessError::RoleAlreadyExists {
                key: role.key().to_string(),
            });
        }
        self.roles.save(&mut scope, &role).await?;
        self.events
            .publish(
                &mut scope,
                &[AccessEvent::RoleCreated {
                    tenant_id: Some(cmd.tenant_id),
                    role_id: role.id().as_uuid(),
                    key: role.key().to_string(),
                    permissions: permissions.iter().cloned().collect(),
                }],
            )
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::User,
                    actor_id: Some(actor),
                    on_behalf_of,
                    tenant_id: Some(cmd.tenant_id),
                    action: "access.role_created".to_string(),
                    resource_type: Some("role".to_string()),
                    resource_id: Some(role.key().to_string()),
                    before: None,
                    after: Some(serde_json::json!({
                        "name": role.name(),
                        "permissions": permissions,
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;
        Ok(role)
    }
}

pub struct UpdateCustomRoleCommand {
    pub role_id: RoleId,
    pub name: Option<String>,
    pub permissions: Option<Vec<String>>,
}

pub struct UpdateCustomRole {
    tx_manager: TxManager,
    roles: Arc<dyn RoleRepository>,
    authorizer: Arc<dyn Authorizer>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl UpdateCustomRole {
    pub fn new(
        tx_manager: TxManager,
        roles: Arc<dyn RoleRepository>,
        authorizer: Arc<dyn Authorizer>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            roles,
            authorizer,
            events,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        on_behalf_of: Option<Uuid>,
        cmd: UpdateCustomRoleCommand,
    ) -> Result<Role, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("update_role", "tenant-admin")
            .await
            .map_err(storage)?;
        let existing = self
            .roles
            .find_by_id(&mut scope, cmd.role_id)
            .await?
            .ok_or_else(|| AccessError::RoleNotFound {
                key: cmd.role_id.to_string(),
            })?;
        if existing.is_system() || existing.scope() != RoleScope::Tenant {
            return Err(AccessError::SystemRoleImmutable {
                key: existing.key().to_string(),
            });
        }
        let Some(tenant_id) = existing.tenant_id() else {
            return Err(AccessError::SystemRoleImmutable {
                key: existing.key().to_string(),
            });
        };

        let permissions = match cmd.permissions {
            Some(raw) => {
                let normalized = normalize_permissions(&raw)?;
                ensure_actor_holds(&self.authorizer, actor, tenant_id, &normalized).await?;
                normalized
            }
            None => existing.permissions().clone(),
        };

        let added: Vec<String> = permissions
            .difference(existing.permissions())
            .cloned()
            .collect();
        let removed: Vec<String> = existing
            .permissions()
            .difference(&permissions)
            .cloned()
            .collect();

        let updated = Role::new(
            existing.id(),
            RoleScope::Tenant,
            Some(tenant_id),
            existing.key().to_string(),
            cmd.name.unwrap_or_else(|| existing.name().to_string()),
            permissions.clone(),
            false,
            existing.created_at(),
        )?;
        self.roles.save(&mut scope, &updated).await?;
        self.events
            .publish(
                &mut scope,
                &[AccessEvent::RoleUpdated {
                    tenant_id: Some(tenant_id),
                    role_id: updated.id().as_uuid(),
                    key: updated.key().to_string(),
                    added_permissions: added,
                    removed_permissions: removed,
                }],
            )
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::User,
                    actor_id: Some(actor),
                    on_behalf_of,
                    tenant_id: Some(tenant_id),
                    action: "access.role_updated".to_string(),
                    resource_type: Some("role".to_string()),
                    resource_id: Some(updated.key().to_string()),
                    before: Some(serde_json::json!({
                        "name": existing.name(),
                        "permissions": existing.permissions(),
                    })),
                    after: Some(serde_json::json!({
                        "name": updated.name(),
                        "permissions": permissions,
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;
        Ok(updated)
    }
}

pub struct DeleteCustomRole {
    tx_manager: TxManager,
    roles: Arc<dyn RoleRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl DeleteCustomRole {
    pub fn new(
        tx_manager: TxManager,
        roles: Arc<dyn RoleRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            roles,
            events,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        on_behalf_of: Option<Uuid>,
        role_id: RoleId,
    ) -> Result<(), AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("delete_role", "tenant-admin")
            .await
            .map_err(storage)?;
        let existing = self
            .roles
            .find_by_id(&mut scope, role_id)
            .await?
            .ok_or_else(|| AccessError::RoleNotFound {
                key: role_id.to_string(),
            })?;
        if existing.is_system() || existing.scope() != RoleScope::Tenant {
            return Err(AccessError::SystemRoleImmutable {
                key: existing.key().to_string(),
            });
        }
        self.roles.delete(&mut scope, role_id).await?;
        self.events
            .publish(
                &mut scope,
                &[AccessEvent::RoleDeleted {
                    tenant_id: existing.tenant_id(),
                    role_id: existing.id().as_uuid(),
                    key: existing.key().to_string(),
                    permissions: existing.permissions().iter().cloned().collect(),
                }],
            )
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::User,
                    actor_id: Some(actor),
                    on_behalf_of,
                    tenant_id: existing.tenant_id(),
                    action: "access.role_deleted".to_string(),
                    resource_type: Some("role".to_string()),
                    resource_id: Some(existing.key().to_string()),
                    before: Some(serde_json::json!({
                        "name": existing.name(),
                        "permissions": existing.permissions(),
                    })),
                    after: None,
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;
        Ok(())
    }
}

/// Lists roles: the platform's full catalog (platform operator console) or
/// the roles assignable within a tenant.
pub struct ListRoles {
    tx_manager: TxManager,
    roles: Arc<dyn RoleRepository>,
}

impl ListRoles {
    pub fn new(tx_manager: TxManager, roles: Arc<dyn RoleRepository>) -> Self {
        Self { tx_manager, roles }
    }

    pub async fn execute_platform(&self) -> Result<Vec<Role>, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_roles", "platform-admin")
            .await
            .map_err(storage)?;
        let roles = self.roles.list_all(&mut scope).await?;
        scope.commit().await.map_err(storage)?;
        Ok(roles)
    }

    pub async fn execute_tenant(&self, tenant_id: Uuid) -> Result<Vec<Role>, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_roles", "tenant-admin")
            .await
            .map_err(storage)?;
        let roles = self.roles.assignable_to(&mut scope, tenant_id).await?;
        scope.commit().await.map_err(storage)?;
        Ok(roles)
    }
}
