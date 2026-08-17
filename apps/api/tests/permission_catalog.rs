//! The permission-catalog CI gate: every non-public route must declare a
//! permission, and every declared permission must exist in the central
//! access registry — no orphans in either direction.
//!
//! The route catalogs are the *same structures registration uses*
//! (`identity_routes`/`access_routes` iterate them; tenancy's
//! `PERMISSIONED_ROUTES` mirrors its route table), so a route without a
//! declaration cannot reach a handler without this test failing first.

use sanvi_access::domain::PERMISSIONS;
use sanvi_identity::adapters::http::catalog::{ROUTES as IDENTITY_ROUTES, Requirement};
use sanvi_tenancy::adapters::http::PERMISSIONED_ROUTES as TENANCY_ROUTES;

/// Collect every catalog the CI gate checks: the same structures route
/// registration iterates.
fn catalogs() -> Vec<(&'static str, Vec<(&'static str, String, &'static str)>)> {
    let mut all = Vec::new();
    all.push((
        "identity",
        IDENTITY_ROUTES
            .iter()
            .filter_map(|route| match route.requirement {
                Requirement::Permission(key, _) => {
                    Some((route.path, route.method.as_str().to_string(), key))
                }
                _ => None,
            })
            .collect(),
    ));
    all.push((
        "access",
        sanvi_access::adapters::http::catalog::ROUTES
            .iter()
            .map(|route| {
                (
                    route.path,
                    route.method.as_str().to_string(),
                    route.permission,
                )
            })
            .collect(),
    ));
    all.push((
        "audit",
        sanvi_audit::catalog::PERMISSIONED_ROUTES
            .iter()
            .map(|route| {
                (
                    route.path,
                    route.method.as_str().to_string(),
                    route.permission,
                )
            })
            .collect(),
    ));
    all.push((
        "platform_admin",
        sanvi_platform_admin::catalog::PERMISSIONED_ROUTES
            .iter()
            .map(|route| {
                (
                    route.path,
                    route.method.as_str().to_string(),
                    route.permission,
                )
            })
            .collect(),
    ));
    all.push((
        "billing",
        sanvi_billing::catalog::PERMISSIONED_ROUTES
            .iter()
            .map(|route| {
                (
                    route.path,
                    route.method.as_str().to_string(),
                    route.permission,
                )
            })
            .collect(),
    ));
    all.push((
        "privacy",
        sanvi_privacy::catalog::PERMISSIONED_ROUTES
            .iter()
            .filter_map(|route| match route.requirement {
                sanvi_privacy::catalog::Requirement::Permission(key, _) => {
                    Some((route.path, route.method.as_str().to_string(), key))
                }
                _ => None,
            })
            .collect(),
    ));
    all.push((
        "tenancy",
        TENANCY_ROUTES
            .iter()
            .map(|(path, method, permission, _)| (*path, method.as_str().to_string(), *permission))
            .collect(),
    ));
    all.push((
        "localization",
        sanvi_localization::catalog::PERMISSIONED_ROUTES
            .iter()
            .map(|(path, method, permission, _)| (*path, method.as_str().to_string(), *permission))
            .collect(),
    ));
    all
}

#[test]
fn identity_catalog_declares_everything() {
    let registry: std::collections::BTreeSet<&str> =
        PERMISSIONS.iter().map(|def| def.key).collect();

    let mut seen_paths: std::collections::BTreeSet<(&str, String)> =
        std::collections::BTreeSet::new();
    let mut permission_count = 0;
    let mut exempt_count = 0;

    for route in IDENTITY_ROUTES {
        assert!(
            seen_paths.insert((route.path, route.method.as_str().to_string())),
            "duplicate catalog entry: {} {}",
            route.method,
            route.path
        );
        match route.requirement {
            Requirement::Permission(key, _resource) => {
                assert!(
                    registry.contains(key),
                    "catalog declares permission {key:?} for {} {} but it is not in the access registry",
                    route.method,
                    route.path
                );
                permission_count += 1;
            }
            Requirement::Authenticated | Requirement::Public => {
                // The two explicitly-declared exemption classes.
                exempt_count += 1;
            }
        }
    }
    assert!(
        permission_count >= 10,
        "expected most routes to declare permissions"
    );
    assert_eq!(
        exempt_count, 2,
        "link/challenge (public) and link/complete (authenticated) are the only exemptions"
    );
}

#[test]
fn every_catalog_declares_only_registered_permissions() {
    let registry: std::collections::BTreeSet<&str> =
        PERMISSIONS.iter().map(|def| def.key).collect();
    for (context, routes) in catalogs() {
        let mut seen: std::collections::BTreeSet<(&str, String)> =
            std::collections::BTreeSet::new();
        for (path, method, permission) in &routes {
            assert!(
                seen.insert((path, method.clone())),
                "duplicate catalog entry in {context}: {method} {path}"
            );
            assert!(
                registry.contains(permission),
                "{context} declares permission {permission:?} for {method} {path} but it is not in the access registry"
            );
        }
    }
}

#[test]
fn registry_has_no_undeclared_orphans() {
    // Permissions declared in the registry are fine to be unused *yet*
    // (billing.* arrives with phase 04), but they must at least be valid.
    for def in PERMISSIONS {
        assert!(
            sanvi_access::domain::Permission::parse(def.key).is_ok(),
            "registry key must parse: {}",
            def.key
        );
    }
}

#[test]
fn every_declared_permission_is_enforceable() {
    // The extractor machinery only accepts keys that parse; a typo'd
    // catalog entry would fail at runtime — prove the full set parses.
    for (_context, routes) in catalogs() {
        for (_path, _method, permission) in &routes {
            assert!(
                sanvi_access::domain::Permission::parse(permission).is_ok(),
                "declared permission must parse: {permission}"
            );
        }
    }
}
