//! The feature-catalog CI gate: every route that gates on an entitlement
//! must reference a feature declared in the catalog — no orphan feature
//! gates. Declared-but-unused features are fine (they arrive with later
//! phases, exactly like permissions).
//!
//! The route catalogs are the *same structures registration uses*, so a
//! gated route with a typo'd key cannot serve traffic without this test
//! failing first.

use sanvi_access::domain::FEATURE_SEEDS;

#[test]
fn every_feature_gated_route_references_a_declared_feature() {
    let declared: std::collections::BTreeSet<&str> =
        FEATURE_SEEDS.iter().map(|seed| seed.key).collect();

    let mut gated_count = 0;
    for route in sanvi_access::adapters::http::catalog::ROUTES {
        if let Some(feature) = route.feature {
            gated_count += 1;
            assert!(
                declared.contains(feature),
                "route {} {} is gated on feature {feature:?} which is not in the catalog",
                route.method,
                route.path
            );
        }
    }
    for (path, method, features) in sanvi_advertising::catalog::FEATURE_GATED_ROUTES {
        for feature in *features {
            gated_count += 1;
            assert!(
                declared.contains(feature),
                "route {} {} is gated on feature {feature:?} which is not in the catalog",
                method,
                path
            );
        }
    }
    assert!(
        gated_count >= 1,
        "expected at least one feature-gated route to exercise the gate"
    );
}

#[test]
fn every_catalog_feature_key_parses() {
    for seed in FEATURE_SEEDS {
        assert!(
            sanvi_access::domain::FeatureKey::parse(seed.key).is_ok(),
            "catalog feature key must parse: {}",
            seed.key
        );
    }
}

#[test]
fn catalog_keys_are_unique_and_follow_the_naming_convention() {
    let mut seen = std::collections::BTreeSet::new();
    for seed in FEATURE_SEEDS {
        assert!(seen.insert(seed.key), "duplicate feature key {}", seed.key);
        let segments = seed.key.split('.').collect::<Vec<_>>();
        assert!(
            (2..=4).contains(&segments.len()),
            "feature key {} must be <context>.<capability>",
            seed.key
        );
    }
}
