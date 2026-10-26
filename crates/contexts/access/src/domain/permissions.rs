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
        key: "platform.tenant.offboard",
        scope: PermissionScope::Platform,
        description: "Record and read tenant offboarding requests (export-then-purge; the purge itself runs via the CLI with a typed confirmation)",
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
    // --- privacy (phase 05) --------------------------------------------------
    PermissionDef {
        key: "privacy.request.submit",
        scope: PermissionScope::Authenticated,
        description: "Submit data subject requests for the signed-in subject",
    },
    PermissionDef {
        key: "privacy.request.read",
        scope: PermissionScope::Authenticated,
        description: "Read the signed-in subject's own requests",
    },
    PermissionDef {
        key: "privacy.consent.read",
        scope: PermissionScope::Authenticated,
        description: "Read the signed-in subject's consent ledger",
    },
    PermissionDef {
        key: "privacy.consent.update",
        scope: PermissionScope::Authenticated,
        description: "Grant or withdraw consent for the signed-in subject",
    },
    PermissionDef {
        key: "privacy.directive.read",
        scope: PermissionScope::Authenticated,
        description: "Read the resolved purpose directives for the signed-in subject",
    },
    PermissionDef {
        key: "privacy.appeal.create",
        scope: PermissionScope::Authenticated,
        description: "Appeal a refused request (the subject's own)",
    },
    PermissionDef {
        key: "privacy.request.rectify",
        scope: PermissionScope::Authenticated,
        description: "Apply rectification corrections to the signed-in subject's data",
    },
    PermissionDef {
        key: "platform.privacy.request.read",
        scope: PermissionScope::Platform,
        description: "Read the DSR operator view",
    },
    PermissionDef {
        key: "platform.privacy.request.write",
        scope: PermissionScope::Platform,
        description: "Extend deadlines and reject data subject requests",
    },
    PermissionDef {
        key: "platform.privacy.appeal.read",
        scope: PermissionScope::Platform,
        description: "List open appeals",
    },
    PermissionDef {
        key: "platform.privacy.appeal.write",
        scope: PermissionScope::Platform,
        description: "Decide appeals (second-operator review)",
    },
    PermissionDef {
        key: "platform.privacy.jurisdiction.read",
        scope: PermissionScope::Platform,
        description: "Read jurisdiction profiles",
    },
    PermissionDef {
        key: "platform.privacy.jurisdiction.write",
        scope: PermissionScope::Platform,
        description: "Update jurisdiction profiles (audited configuration)",
    },
    PermissionDef {
        key: "platform.privacy.hold.read",
        scope: PermissionScope::Platform,
        description: "List legal holds",
    },
    PermissionDef {
        key: "platform.privacy.hold.write",
        scope: PermissionScope::Platform,
        description: "Apply and release legal holds",
    },
    PermissionDef {
        key: "platform.privacy.incident.read",
        scope: PermissionScope::Platform,
        description: "Read breach incidents and obligations",
    },
    PermissionDef {
        key: "platform.privacy.incident.write",
        scope: PermissionScope::Platform,
        description: "Record breach incidents and discharges",
    },
    PermissionDef {
        key: "platform.privacy.governance.read",
        scope: PermissionScope::Platform,
        description: "Read sub-processors, processing activities, assessments and retention rules",
    },
    PermissionDef {
        key: "platform.privacy.governance.write",
        scope: PermissionScope::Platform,
        description: "Manage sub-processors, processing activities, assessments and retention rules",
    },
    PermissionDef {
        key: "tenant.privacy.request.write",
        scope: PermissionScope::Tenant,
        description: "Submit data subject requests for the tenant's end users (processor role)",
    },
    PermissionDef {
        key: "tenant.privacy.request.read",
        scope: PermissionScope::Tenant,
        description: "Read the tenant's data subject requests",
    },
    // --- localization (phase 06) ---------------------------------------------
    PermissionDef {
        key: "tenant.localization.settings.read",
        scope: PermissionScope::Tenant,
        description: "Read the tenant's locale settings",
    },
    PermissionDef {
        key: "tenant.localization.settings.update",
        scope: PermissionScope::Tenant,
        description: "Change the tenant's default locale, enabled locales and timezone",
    },
    PermissionDef {
        key: "tenant.localization.overrides.read",
        scope: PermissionScope::Tenant,
        description: "Read the tenant's copy overrides",
    },
    PermissionDef {
        key: "tenant.localization.overrides.update",
        scope: PermissionScope::Tenant,
        description: "Edit the tenant's copy overrides",
    },
    PermissionDef {
        key: "platform.localization.translations.read",
        scope: PermissionScope::Platform,
        description: "Read the platform catalog overrides",
    },
    PermissionDef {
        key: "platform.localization.translations.write",
        scope: PermissionScope::Platform,
        description: "Edit the platform catalog overrides",
    },
    // --- theming (phase 07) ---------------------------------------------------
    PermissionDef {
        key: "tenant.theming.read",
        scope: PermissionScope::Tenant,
        description: "Read the tenant's theme draft, available themes and previews",
    },
    PermissionDef {
        key: "tenant.theming.write",
        scope: PermissionScope::Tenant,
        description: "Edit the tenant's theme draft (overrides, assets, custom CSS)",
    },
    PermissionDef {
        key: "tenant.theming.publish",
        scope: PermissionScope::Tenant,
        description: "Publish the tenant's theme and roll back to previous revisions",
    },
    PermissionDef {
        key: "platform.theme.read",
        scope: PermissionScope::Platform,
        description: "Read the theme registry and version history",
    },
    PermissionDef {
        key: "platform.theme.write",
        scope: PermissionScope::Platform,
        description: "Install and withdraw theme versions",
    },
    // --- domains (phase 08) --------------------------------------------------
    PermissionDef {
        key: "tenant.domains.read",
        scope: PermissionScope::Tenant,
        description: "Read the tenant's custom domains, instructions and statuses",
    },
    PermissionDef {
        key: "tenant.domains.manage",
        scope: PermissionScope::Tenant,
        description: "Claim, verify, promote and remove the tenant's custom domains",
    },
    PermissionDef {
        key: "tenant.domains.purchase",
        scope: PermissionScope::Tenant,
        description: "Search, purchase and manage domain orders (registrar journeys)",
    },
    PermissionDef {
        key: "platform.domain.read",
        scope: PermissionScope::Platform,
        description: "Investigate custom domains across tenants",
    },
    PermissionDef {
        key: "platform.domain.write",
        scope: PermissionScope::Platform,
        description: "Force-release custom domains (abuse, disputes, take-downs)",
    },
    // --- tenant payments (phase 09) -------------------------------------------
    // `payments.refund` is deliberately NOT in the default staff/member
    // roles: moving money back out is owner/admin territory, and every
    // refund is audited with actor and reason.
    PermissionDef {
        key: "payments.read",
        scope: PermissionScope::Tenant,
        description: "Read the provider catalog, connections, payments, payouts and disputes",
    },
    PermissionDef {
        key: "payments.manage",
        scope: PermissionScope::Tenant,
        description: "Connect, onboard and disconnect payment providers for the tenant",
    },
    PermissionDef {
        key: "payments.checkout",
        scope: PermissionScope::Tenant,
        description: "Create checkout sessions on the tenant's connected account",
    },
    PermissionDef {
        key: "payments.refund",
        scope: PermissionScope::Tenant,
        description: "Issue full and partial refunds on the tenant's payments (audited)",
    },
    // --- advertising (phase 10) -------------------------------------------
    PermissionDef {
        key: "advertising.read",
        scope: PermissionScope::Tenant,
        description: "Read advertising platforms, connections, campaigns and budget status",
    },
    PermissionDef {
        key: "advertising.connect",
        scope: PermissionScope::Tenant,
        description: "Connect and disconnect advertising accounts",
    },
    PermissionDef {
        key: "advertising.campaign.read",
        scope: PermissionScope::Tenant,
        description: "Read advertising campaigns and delivery state",
    },
    PermissionDef {
        key: "advertising.campaign.write",
        scope: PermissionScope::Tenant,
        description: "Create and change advertising campaigns (audited and idempotent)",
    },
    PermissionDef {
        key: "advertising.metrics.read",
        scope: PermissionScope::Tenant,
        description: "Read advertising metrics, conversions and ROAS",
    },
    PermissionDef {
        key: "advertising.budget.manage",
        scope: PermissionScope::Tenant,
        description: "Set advertising spend caps and auto-pause guardrails",
    },
    PermissionDef {
        key: "media.assets.manage",
        scope: PermissionScope::Tenant,
        description: "Upload, read and delete media assets, and mint delivery URLs, via the tenant's media gateway",
    },
    // Product services: checked by the product service itself from the
    // delegated permissions, never by a platform route.
    PermissionDef {
        key: "ecommerce.store.read",
        scope: PermissionScope::Tenant,
        description: "List and read the tenant's ecommerce stores",
    },
    PermissionDef {
        key: "ecommerce.store.manage",
        scope: PermissionScope::Tenant,
        description: "Open and change ecommerce stores, within the plan's store limit",
    },
    PermissionDef {
        key: "ecommerce.product.read",
        scope: PermissionScope::Tenant,
        description: "List and read products in the tenant's ecommerce stores",
    },
    PermissionDef {
        key: "ecommerce.product.manage",
        scope: PermissionScope::Tenant,
        description: "Create and change products in the tenant's ecommerce stores",
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
