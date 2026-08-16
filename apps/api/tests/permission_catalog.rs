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
fn access_catalog_declares_everything() {
    let registry: std::collections::BTreeSet<&str> =
        PERMISSIONS.iter().map(|def| def.key).collect();
    for route in sanvi_access::adapters::http::catalog::ROUTES {
        assert!(
            registry.contains(route.permission),
            "access catalog declares permission {:?} for {} {} but it is not in the registry",
            route.permission,
            route.method,
            route.path
        );
    }
}

#[test]
fn tenancy_catalog_declares_everything() {
    let registry: std::collections::BTreeSet<&str> =
        PERMISSIONS.iter().map(|def| def.key).collect();
    for (_path, _method, permission, _resource) in TENANCY_ROUTES {
        assert!(
            registry.contains(permission),
            "tenancy declares permission {permission:?} but it is not in the access registry"
        );
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
    let mut keys: Vec<&str> = IDENTITY_ROUTES
        .iter()
        .filter_map(|route| match route.requirement {
            Requirement::Permission(key, _) => Some(key),
            _ => None,
        })
        .collect();
    keys.extend(
        sanvi_access::adapters::http::catalog::ROUTES
            .iter()
            .map(|route| route.permission),
    );
    keys.extend(
        TENANCY_ROUTES
            .iter()
            .map(|(_path, _method, permission, _resource)| *permission),
    );
    for key in keys {
        assert!(
            sanvi_access::domain::Permission::parse(key).is_ok(),
            "declared permission must parse: {key}"
        );
    }
}
