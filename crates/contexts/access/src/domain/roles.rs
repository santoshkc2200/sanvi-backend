//! The role catalog: platform roles, tenant system roles (templates shared
//! by every tenant) and per-tenant custom roles.

use chrono::{DateTime, Utc};
use sanvi_kernel::Id;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::AccessError;
use crate::domain::permissions::PermissionScope;

pub enum RoleTag {}
pub type RoleId = Id<RoleTag>;

/// Where a role applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoleScope {
    /// Platform-wide (`role:platform:{key}` in Keto).
    Platform,
    /// Tenant-scoped (`role:{tenant_id}:{key}`).
    Tenant,
}

impl RoleScope {
    pub fn parse(raw: &str) -> Result<Self, AccessError> {
        match raw {
            "platform" => Ok(Self::Platform),
            "tenant" => Ok(Self::Tenant),
            other => Err(AccessError::InvalidRoleScope(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Tenant => "tenant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    id: RoleId,
    scope: RoleScope,
    /// Tenant id for custom tenant roles; NULL for platform roles and the
    /// system tenant templates.
    tenant_id: Option<uuid::Uuid>,
    key: String,
    name: String,
    permissions: std::collections::BTreeSet<String>,
    is_system: bool,
    created_at: DateTime<Utc>,
}

impl Role {
    pub fn new(
        id: RoleId,
        scope: RoleScope,
        tenant_id: Option<uuid::Uuid>,
        key: String,
        name: String,
        permissions: std::collections::BTreeSet<String>,
        is_system: bool,
        now: DateTime<Utc>,
    ) -> Result<Self, AccessError> {
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty()
            || key.len() > 63
            || !key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(AccessError::InvalidRoleKey(key));
        }
        if scope == RoleScope::Platform && tenant_id.is_some() {
            return Err(AccessError::InvalidRoleScope(
                "platform roles cannot be tenant-bound".to_string(),
            ));
        }
        if !is_system && tenant_id.is_none() {
            return Err(AccessError::InvalidRoleScope(
                "custom roles must be tenant-bound".to_string(),
            ));
        }
        for permission in &permissions {
            if !crate::domain::permissions::PERMISSIONS
                .iter()
                .any(|def| def.key == *permission)
            {
                return Err(AccessError::UnknownPermission(permission.clone()));
            }
        }
        Ok(Self {
            id,
            scope,
            tenant_id,
            key,
            name,
            permissions,
            is_system,
            created_at: now,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: RoleId,
        scope: RoleScope,
        tenant_id: Option<uuid::Uuid>,
        key: String,
        name: String,
        permissions: std::collections::BTreeSet<String>,
        is_system: bool,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            scope,
            tenant_id,
            key,
            name,
            permissions,
            is_system,
            created_at,
        }
    }

    pub fn id(&self) -> RoleId {
        self.id
    }
    pub fn scope(&self) -> RoleScope {
        self.scope
    }
    pub fn tenant_id(&self) -> Option<uuid::Uuid> {
        self.tenant_id
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn permissions(&self) -> &std::collections::BTreeSet<String> {
        &self.permissions
    }
    pub fn is_system(&self) -> bool {
        self.is_system
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    /// The Keto relation-tuple object for this role:
    /// `platform:{key}` or `{tenant_id}:{key}`. System tenant templates
    /// (tenant_id NULL) have no object of their own — they materialize per
    /// tenant at authorization time.
    pub fn tuple_object(&self) -> String {
        match self.scope {
            RoleScope::Platform => format!("platform:{}", self.key),
            RoleScope::Tenant => format!(
                "{}:{}",
                self.tenant_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "template".to_string()),
                self.key
            ),
        }
    }
}

/// One seed entry: scope + key + permissions. System tenant roles are
/// templates (tenant_id NULL in the catalog) materialized per tenant at
/// authorization time; platform roles are global.
pub struct RoleSeed {
    pub scope: RoleScope,
    pub key: &'static str,
    pub name: &'static str,
    pub permissions: &'static [&'static str],
}

/// The system role catalog, seeded idempotently by `apps/cli seed roles`.
pub const SYSTEM_ROLE_SEEDS: &[RoleSeed] = &[
    RoleSeed {
        scope: RoleScope::Platform,
        key: "platform_owner",
        name: "Platform Owner",
        permissions: &["platform.tenant.manage", "platform.role.manage"],
    },
    RoleSeed {
        scope: RoleScope::Platform,
        key: "platform_operator",
        name: "Platform Operator",
        permissions: &["platform.tenant.manage"],
    },
    RoleSeed {
        scope: RoleScope::Platform,
        key: "platform_support",
        name: "Platform Support",
        permissions: &[],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_owner",
        name: "Tenant Owner",
        permissions: &[
            "identity.member.read",
            "identity.member.grant",
            "identity.member.remove",
            "identity.member.invite",
            "identity.invitation.manage",
            "tenancy.settings.read",
            "tenancy.settings.update",
            "billing.subscription.read",
            "billing.subscription.update",
        ],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_admin",
        name: "Tenant Admin",
        permissions: &[
            "identity.member.read",
            "identity.member.grant",
            "identity.member.remove",
            "identity.member.invite",
            "identity.invitation.manage",
            "tenancy.settings.read",
            "tenancy.settings.update",
        ],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_member",
        name: "Tenant Member",
        permissions: &["identity.member.read", "tenancy.settings.read"],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_billing",
        name: "Tenant Billing",
        permissions: &[
            "identity.member.read",
            "billing.subscription.read",
            "billing.subscription.update",
        ],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_viewer",
        name: "Tenant Viewer",
        permissions: &["identity.member.read", "tenancy.settings.read"],
    },
];

/// Convenience for tests and the access adapter's role-key lookups.
pub fn seed_by_key(key: &str) -> Option<&'static RoleSeed> {
    SYSTEM_ROLE_SEEDS.iter().find(|seed| seed.key == key)
}

/// Verify the seeds reference only declared permissions and don't assign
/// permissions outside their scope's reachability rules.
pub fn validate_seeds() -> Result<(), AccessError> {
    for seed in SYSTEM_ROLE_SEEDS {
        for permission in seed.permissions {
            let def = crate::domain::permissions::lookup(permission)
                .ok_or_else(|| AccessError::UnknownPermission(permission.to_string()))?;
            let allowed = match (seed.scope, def.scope) {
                (RoleScope::Platform, PermissionScope::Platform) => true,
                (RoleScope::Tenant, PermissionScope::Tenant) => true,
                // Authenticated-scope permissions are implicit for everyone;
                // assigning them to a role would be misleading.
                _ => false,
            };
            if !allowed {
                return Err(AccessError::PermissionScopeMismatch {
                    permission: permission.to_string(),
                    scope: seed.scope.as_str().to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_reference_only_declared_permissions_in_matching_scopes() {
        validate_seeds().unwrap();
    }

    #[test]
    fn seed_keys_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for seed in SYSTEM_ROLE_SEEDS {
            assert!(seen.insert(seed.key), "duplicate seed {}", seed.key);
        }
    }

    #[test]
    fn role_validation_rejects_unknown_permissions() {
        let mut permissions = std::collections::BTreeSet::new();
        permissions.insert("no.such.permission".to_string());
        assert!(matches!(
            Role::new(
                RoleId::new(),
                RoleScope::Tenant,
                Some(uuid::Uuid::now_v7()),
                "custom".to_string(),
                "Custom".to_string(),
                permissions,
                false,
                Utc::now(),
            ),
            Err(AccessError::UnknownPermission(_))
        ));
    }

    #[test]
    fn role_validation_rejects_scope_violations() {
        let permissions = std::collections::BTreeSet::new();
        assert!(
            Role::new(
                RoleId::new(),
                RoleScope::Platform,
                Some(uuid::Uuid::now_v7()),
                "custom".to_string(),
                "Custom".to_string(),
                permissions.clone(),
                false,
                Utc::now(),
            )
            .is_err()
        );
        assert!(
            Role::new(
                RoleId::new(),
                RoleScope::Tenant,
                None,
                "custom".to_string(),
                "Custom".to_string(),
                permissions,
                false,
                Utc::now(),
            )
            .is_err()
        );
    }

    #[test]
    fn tuple_object_shapes() {
        let tenant_id = uuid::Uuid::now_v7();
        let platform = Role::from_parts(
            RoleId::new(),
            RoleScope::Platform,
            None,
            "platform_owner".to_string(),
            "Platform Owner".to_string(),
            Default::default(),
            true,
            Utc::now(),
        );
        assert_eq!(platform.tuple_object(), "platform:platform_owner");
        let tenant = Role::from_parts(
            RoleId::new(),
            RoleScope::Tenant,
            Some(tenant_id),
            "tenant_owner".to_string(),
            "Tenant Owner".to_string(),
            Default::default(),
            false,
            Utc::now(),
        );
        assert_eq!(tenant.tuple_object(), format!("{tenant_id}:tenant_owner"));
    }
}
