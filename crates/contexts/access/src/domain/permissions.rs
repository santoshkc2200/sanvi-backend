//! The central permission registry: every permission the platform
//! recognizes, in one place. The frontend, docs and Keto seed derive from
//! this list; the CI test proves no route checks a permission that is not
//! declared here and vice versa.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::AccessError;

/// Naming convention: `<context>.<resource>.<action>`, all lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct Permission(String);

impl Permission {
    pub fn parse(raw: &str) -> Result<Self, AccessError> {
        let key = raw.trim().to_ascii_lowercase();
        let mut valid = key.len() >= 7;
        let segments = key.split('.');
        let mut count = 0;
        for segment in segments {
            count += 1;
            if segment.is_empty()
                || !segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            {
                valid = false;
            }
        }
        if count > 4 {
            valid = false;
        }
        if !valid {
            return Err(AccessError::InvalidPermission(raw.to_string()));
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Who a permission applies to, which decides how the check resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    /// Checked against platform roles (`role:platform:{key}` in Keto).
    Platform,
    /// Checked against the tenant's roles (`role:{tenant_id}:{key}`).
    Tenant,
    /// Granted to every authenticated subject — the local fast path, no
    /// Keto roundtrip.
    Authenticated,
}

impl PermissionScope {
    pub fn parse(raw: &str) -> Result<Self, AccessError> {
        match raw {
            "platform" => Ok(Self::Platform),
            "tenant" => Ok(Self::Tenant),
            "authenticated" => Ok(Self::Authenticated),
            other => Err(AccessError::InvalidRoleScope(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Tenant => "tenant",
            Self::Authenticated => "authenticated",
        }
    }
}

/// One registry entry: key, scope, and a human-readable description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PermissionDef {
    pub key: &'static str,
    pub scope: PermissionScope,
    pub description: &'static str,
}

/// The registry. Permissions for later phases (billing) are declared now so
/// contracts and seeds are stable; enforcement arrives with their contexts.
pub const PERMISSIONS: &[PermissionDef] = &[
    // --- platform control plane -------------------------------------------
    PermissionDef {
        key: "platform.tenant.manage",
        scope: PermissionScope::Platform,
        description: "Provision and run tenant lifecycle (activate/suspend/resume/archive)",
    },
    PermissionDef {
        key: "platform.tenant.read",
        scope: PermissionScope::Platform,
        description: "Search tenants and read the operator tenant views",
    },
    PermissionDef {
        key: "platform.role.manage",
        scope: PermissionScope::Platform,
        description: "Author roles and permissions",
    },
    PermissionDef {
        key: "platform.entitlement.write",
        scope: PermissionScope::Platform,
        description: "Grant and revoke entitlement overrides",
    },
    PermissionDef {
        key: "platform.feature.write",
        scope: PermissionScope::Platform,
        description: "Define, update and deprecate the feature catalog",
    },
    PermissionDef {
        key: "platform.impersonate",
        scope: PermissionScope::Platform,
        description: "Start, list and revoke impersonation grants",
    },
    PermissionDef {
        key: "platform.audit.read",
        scope: PermissionScope::Platform,
        description: "Read the platform audit trail",
    },
    PermissionDef {
        key: "platform.approval.read",
        scope: PermissionScope::Platform,
        description: "List four-eyes pending approvals",
    },
    PermissionDef {
        key: "platform.approval.write",
        scope: PermissionScope::Platform,
        description: "Approve or reject four-eyes pending approvals",
    },
    // --- identity & membership ---------------------------------------------
    PermissionDef {
        key: "identity.member.read",
        scope: PermissionScope::Tenant,
        description: "List members and invitations",
    },
    PermissionDef {
        key: "identity.member.grant",
        scope: PermissionScope::Tenant,
        description: "Grant memberships and change member roles",
    },
    PermissionDef {
        key: "identity.member.remove",
        scope: PermissionScope::Tenant,
        description: "Remove members",
    },
    PermissionDef {
        key: "identity.member.invite",
        scope: PermissionScope::Tenant,
        description: "Invite new members by email",
    },
    PermissionDef {
        key: "identity.invitation.manage",
        scope: PermissionScope::Tenant,
        description: "Resend and revoke invitations",
    },
    PermissionDef {
        key: "identity.membership.claim",
        scope: PermissionScope::Authenticated,
        description: "Accept an invitation token for the signed-in account",
    },
    PermissionDef {
        key: "identity.profile.read",
        scope: PermissionScope::Authenticated,
        description: "Read the signed-in user's own profile",
    },
    PermissionDef {
        key: "identity.session.list",
        scope: PermissionScope::Authenticated,
        description: "List the signed-in user's own sessions",
    },
    PermissionDef {
        key: "identity.session.revoke",
        scope: PermissionScope::Authenticated,
        description: "Revoke the signed-in user's own session",
    },
    // --- tenancy settings ---------------------------------------------------
    PermissionDef {
        key: "tenancy.settings.read",
        scope: PermissionScope::Tenant,
        description: "Read tenant settings",
    },
    PermissionDef {
        key: "tenancy.settings.update",
        scope: PermissionScope::Tenant,
        description: "Update tenant settings",
    },
    // --- access -------------------------------------------------------------
    PermissionDef {
        key: "access.permission.list",
        scope: PermissionScope::Authenticated,
        description: "List the permission registry",
    },
    PermissionDef {
        key: "access.role.read",
        scope: PermissionScope::Tenant,
        description: "List roles assignable in the tenant",
    },
    PermissionDef {
        key: "access.role.create",
        scope: PermissionScope::Tenant,
        description: "Create custom tenant roles",
    },
    PermissionDef {
        key: "access.role.update",
        scope: PermissionScope::Tenant,
        description: "Update custom tenant roles",
    },
    PermissionDef {
        key: "access.role.delete",
        scope: PermissionScope::Tenant,
        description: "Delete custom tenant roles",
    },
    PermissionDef {
        key: "tenant.entitlement.read",
        scope: PermissionScope::Tenant,
        description: "Read the tenant's resolved entitlements",
    },
    // --- billing (phase 04) ---------------------------------------------------
    PermissionDef {
        key: "billing.subscription.read",
        scope: PermissionScope::Tenant,
        description: "Read the tenant's subscription",
    },
    PermissionDef {
        key: "billing.subscription.update",
        scope: PermissionScope::Tenant,
        description: "Change the tenant's subscription",
    },
    PermissionDef {
        key: "billing.subscription.create",
        scope: PermissionScope::Tenant,
        description: "Start a subscription checkout",
    },
    PermissionDef {
        key: "billing.invoice.read",
        scope: PermissionScope::Tenant,
        description: "Read the tenant's invoices",
    },
    PermissionDef {
        key: "platform.plan.write",
        scope: PermissionScope::Platform,
        description: "Manage billing plans, prices and entitlement maps",
    },
    PermissionDef {
        key: "platform.subscription.override",
        scope: PermissionScope::Platform,
        description: "Grant and revoke subscription overrides",
    },
];

/// Registry lookup.
pub fn lookup(key: &str) -> Option<&'static PermissionDef> {
    PERMISSIONS.iter().find(|def| def.key == key)
}

/// Every registry key (the orphan test iterates this).
pub fn all_keys() -> impl Iterator<Item = &'static str> {
    PERMISSIONS.iter().map(|def| def.key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_permission_parses() {
        for def in PERMISSIONS {
            assert!(
                Permission::parse(def.key).is_ok(),
                "registry key must parse: {}",
                def.key
            );
        }
    }

    #[test]
    fn registry_has_no_duplicate_keys() {
        let mut seen = std::collections::BTreeSet::new();
        for def in PERMISSIONS {
            assert!(seen.insert(def.key), "duplicate key {}", def.key);
        }
    }

    #[test]
    fn lookup_finds_declared_permissions() {
        assert!(lookup("identity.member.read").is_some());
        assert!(lookup("billing.subscription.update").is_some());
        assert!(lookup("not.a.permission").is_none());
    }

    #[test]
    fn permission_parse_normalizes_and_enforces_the_grammar() {
        // Input is lowercased (like every other identifier in the system).
        assert_eq!(
            Permission::parse("Identity.Member.Read").unwrap().as_str(),
            "identity.member.read"
        );
        for bad in ["", "a.b", "a..b", "a.b.c.d.e.f", "no..dots"] {
            assert!(Permission::parse(bad).is_err(), "should reject {bad:?}");
        }
    }
}
