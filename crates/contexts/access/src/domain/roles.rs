//! The role catalog: platform roles, tenant system roles (templates shared
//! by every tenant) and per-tenant custom roles.

use chrono::{DateTime, Utc};
pub use sanvi_kernel::{RoleId, TenantId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::AccessError;
use crate::domain::permissions::PermissionScope;

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
    tenant_id: Option<TenantId>,
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
        tenant_id: Option<TenantId>,
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
        tenant_id: Option<TenantId>,
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
    pub fn tenant_id(&self) -> Option<TenantId> {
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
        permissions: &[
            "platform.tenant.manage",
            "platform.tenant.read",
            "platform.role.manage",
            "platform.entitlement.write",
            "platform.feature.write",
            "platform.impersonate",
            "platform.audit.read",
            "platform.approval.read",
            "platform.approval.write",
            "platform.plan.write",
            "platform.subscription.override",
        ],
    },
    RoleSeed {
        scope: RoleScope::Platform,
        key: "platform_operator",
        name: "Platform Operator",
        permissions: &[
            "platform.tenant.manage",
            "platform.tenant.read",
            "platform.entitlement.write",
            "platform.audit.read",
            "platform.plan.write",
            "platform.subscription.override",
        ],
    },
    RoleSeed {
        scope: RoleScope::Platform,
        key: "platform_support",
        name: "Platform Support",
        permissions: &["platform.tenant.read", "platform.impersonate"],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_owner",
        name: "Tenant Owner",
        permissions: &[
            "tenant.domains.read",
            "tenant.domains.manage",
            "tenant.domains.purchase",
            "tenant.accounting.read",
            "tenant.accounting.manage",
            "identity.member.read",
            "identity.member.grant",
            "identity.member.remove",
            "identity.member.invite",
            "identity.invitation.manage",
            "tenancy.settings.read",
            "tenancy.settings.update",
            "access.role.read",
            "access.role.create",
            "access.role.update",
            "access.role.delete",
            "tenant.entitlement.read",
            "billing.subscription.read",
            "billing.subscription.update",
            "billing.subscription.create",
            "billing.invoice.read",
            // Phase 09: owners run the money — including refunds.
            "payments.read",
            "payments.manage",
            "payments.checkout",
            "payments.refund",
            "advertising.read",
            "advertising.connect",
            "advertising.campaign.read",
            "advertising.campaign.write",
            "advertising.metrics.read",
            "advertising.budget.manage",
            "media.assets.manage",
            "ecommerce.store.read",
            "ecommerce.store.manage",
            "ecommerce.product.read",
            "ecommerce.product.manage",
            "ecommerce.import.read",
            "ecommerce.import.manage",
        ],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_admin",
        name: "Tenant Admin",
        permissions: &[
            "tenant.accounting.read",
            "tenant.accounting.manage",
            "identity.member.read",
            "identity.member.grant",
            "identity.member.remove",
            "identity.member.invite",
            "identity.invitation.manage",
            "tenancy.settings.read",
            "tenancy.settings.update",
            "access.role.read",
            "access.role.create",
            "access.role.update",
            "access.role.delete",
            "tenant.entitlement.read",
            // Phase 09: admins connect providers and read money; refunds
            // stay owner-only by default (grantable per-tenant).
            "payments.read",
            "payments.manage",
            "payments.checkout",
            // Phase 10: tenant admins operate the advertising workspace;
            // regular members remain unable to connect platforms or manage
            // budgets by default.
            "advertising.read",
            "advertising.connect",
            "advertising.campaign.read",
            "advertising.campaign.write",
            "advertising.metrics.read",
            "advertising.budget.manage",
            "media.assets.manage",
            "ecommerce.store.read",
            "ecommerce.store.manage",
            "ecommerce.product.read",
            "ecommerce.product.manage",
            "ecommerce.import.read",
            "ecommerce.import.manage",
        ],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_member",
        name: "Tenant Member",
        permissions: &[
            "identity.member.read",
            "tenancy.settings.read",
            "ecommerce.store.read",
            "ecommerce.product.read",
        ],
    },
    RoleSeed {
        scope: RoleScope::Tenant,
        key: "tenant_billing",
        name: "Tenant Billing",
        permissions: &[
            "identity.member.read",
            "billing.subscription.read",
            "billing.subscription.update",
            "billing.subscription.create",
            "billing.invoice.read",
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
    fn refund_stays_owner_only_in_the_default_roles() {
        // Phase 09's deliberate call: `payments.refund` moves money back
        // out, so the default member/staff roles never carry it — only
        // tenant_owner (admins can be granted it per-tenant).
        for seed in SYSTEM_ROLE_SEEDS {
            if seed.key == "tenant_owner" {
                continue;
            }
            assert!(
                !seed.permissions.contains(&"payments.refund"),
                "{} must not refund by default",
                seed.key
            );
        }
        assert!(
            seed_by_key("tenant_owner")
                .unwrap()
                .permissions
                .contains(&"payments.refund")
        );
    }

    #[test]
    fn owner_can_manage_and_purchase_domains_without_granting_members_access() {
        let owner = seed_by_key("tenant_owner").unwrap().permissions;
        let member = seed_by_key("tenant_member").unwrap().permissions;
        for permission in [
            "tenant.domains.read",
            "tenant.domains.manage",
            "tenant.domains.purchase",
        ] {
            assert!(owner.contains(&permission), "owner needs {permission}");
            assert!(
                !member.contains(&permission),
                "member must not have {permission}"
            );
        }
    }

    #[test]
    fn advertising_permissions_are_reachable_from_admin_roles() {
        let owner = seed_by_key("tenant_owner").unwrap().permissions;
        let admin = seed_by_key("tenant_admin").unwrap().permissions;
        for permission in [
            "advertising.read",
            "advertising.connect",
            "advertising.campaign.read",
            "advertising.campaign.write",
            "advertising.metrics.read",
            "advertising.budget.manage",
            "media.assets.manage",
        ] {
            assert!(owner.contains(&permission), "owner needs {permission}");
        }
        for permission in [
            "advertising.read",
            "advertising.connect",
            "advertising.campaign.read",
            "advertising.campaign.write",
            "advertising.metrics.read",
            "advertising.budget.manage",
            "media.assets.manage",
        ] {
            assert!(admin.contains(&permission), "admin needs {permission}");
        }
        let member = seed_by_key("tenant_member").unwrap().permissions;
        assert!(!member.contains(&"advertising.connect"));
        assert!(!member.contains(&"advertising.budget.manage"));
        assert!(!member.contains(&"media.assets.manage"));
    }

    #[test]
    fn import_permissions_are_reachable_from_admin_and_owner_roles_only() {
        let owner = seed_by_key("tenant_owner").unwrap().permissions;
        let admin = seed_by_key("tenant_admin").unwrap().permissions;
        let member = seed_by_key("tenant_member").unwrap().permissions;
        for permission in ["ecommerce.import.read", "ecommerce.import.manage"] {
            assert!(owner.contains(&permission), "owner needs {permission}");
            assert!(admin.contains(&permission), "admin needs {permission}");
            assert!(
                !member.contains(&permission),
                "member must not have {permission}"
            );
        }
    }

    #[test]
    fn billing_permissions_are_reachable_from_default_roles() {
        for role in ["platform_owner", "platform_operator"] {
            let permissions = seed_by_key(role).unwrap().permissions;
            assert!(permissions.contains(&"platform.plan.write"));
            assert!(permissions.contains(&"platform.subscription.override"));
        }
        for role in ["tenant_owner", "tenant_billing"] {
            let permissions = seed_by_key(role).unwrap().permissions;
            assert!(permissions.contains(&"billing.subscription.create"));
            assert!(permissions.contains(&"billing.invoice.read"));
        }
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
                RoleId::try_from(1_i64).unwrap(),
                RoleScope::Tenant,
                Some(TenantId::try_from(101_i64).unwrap()),
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
                RoleId::try_from(1_i64).unwrap(),
                RoleScope::Platform,
                Some(TenantId::try_from(101_i64).unwrap()),
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
                RoleId::try_from(1_i64).unwrap(),
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
        let tenant_id = TenantId::try_from(101_i64).unwrap();
        let platform = Role::from_parts(
            RoleId::try_from(1_i64).unwrap(),
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
            RoleId::try_from(2_i64).unwrap(),
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

    #[test]
    fn owner_and_admin_manage_accounting_member_does_not() {
        for (role, expected) in [
            ("tenant_owner", true),
            ("tenant_admin", true),
            ("tenant_member", false),
        ] {
            let seed = SYSTEM_ROLE_SEEDS.iter().find(|r| r.key == role).unwrap();
            for key in ["tenant.accounting.read", "tenant.accounting.manage"] {
                assert_eq!(seed.permissions.contains(&key), expected, "{role} {key}");
            }
        }
    }
}
