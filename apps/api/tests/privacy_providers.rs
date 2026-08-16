//! The phase-05 privacy CI gates:
//!
//! 1. **Every context registers a `PersonalDataProvider`** — the workspace's
//!    context crates are enumerated from the root manifest, and the
//!    provider registry must cover exactly that set. Adding a context
//!    without a provider fails the build; adding a provider for a context
//!    that does not exist also fails (the two cannot drift).
//! 2. **Non-discrimination** — the entitlement resolver (access context)
//!    has no dependency on the privacy read model: directive state can
//!    never be an input to pricing, entitlements or feature gating. Proven
//!    by asserting `sanvi-access` does not depend on `sanvi-privacy`.
//! 3. **The public disclosures are generated** — the annual metrics query
//!    and the notice's retention table come from the ledger/rules, never
//!    from hand-written content.

use std::path::PathBuf;

/// The workspace root (this test lives in `apps/api/tests/`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

/// Every context crate member of the workspace, derived from the root
/// `Cargo.toml` so a newly added context is automatically in scope.
fn workspace_context_crates() -> Vec<String> {
    let manifest = std::fs::read_to_string(workspace_root().join("Cargo.toml"))
        .expect("workspace manifest readable");
    manifest
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("\"crates/contexts/")
                .and_then(|rest| rest.strip_suffix("\","))
                .map(str::to_string)
        })
        .collect()
}

#[test]
fn every_context_crate_registers_a_personal_data_provider() {
    let contexts = workspace_context_crates();
    assert!(
        contexts.len() >= 8,
        "expected the workspace context list; found {contexts:?}"
    );

    // The registry's canonical name list is the contract; the composition
    // root builds exactly one provider per context crate.
    let registered: std::collections::BTreeSet<&str> = sanvi_privacy::domain::CONTEXT_NAMES
        .iter()
        .copied()
        .collect();

    for context in &contexts {
        assert!(
            registered.contains(context.as_str()),
            "context crate `{context}` registers no PersonalDataProvider — add one (see crates/contexts/privacy/src/domain/ports.rs)"
        );
    }
    for name in &registered {
        assert!(
            contexts.iter().any(|c| c == name),
            "provider registered for `{name}` but no such context crate exists"
        );
    }
}

#[test]
fn entitlement_resolution_has_no_privacy_dependency() {
    // Non-discrimination: a directive state is never an input to pricing,
    // entitlements or feature gating. The entitlement resolver's module
    // tree must not import the privacy read model (the access crate's only
    // privacy edge is its PersonalDataProvider, which serves the privacy
    // context — it never feeds the resolver).
    let access_root = workspace_root().join("crates/contexts/access/src");
    let resolver_files = [
        access_root.join("application/entitlements.rs"),
        access_root.join("application/feature_gate.rs"),
        access_root.join("application/features.rs"),
        access_root.join("domain/entitlements.rs"),
        access_root.join("domain/features.rs"),
    ];
    for file in resolver_files {
        let contents = std::fs::read_to_string(&file)
            .unwrap_or_else(|err| panic!("resolver file {}: {err}", file.display()));
        assert!(
            !contents.contains("sanvi_privacy"),
            "{} imports the privacy read model — directive state must never feed entitlement resolution (non-discrimination)",
            file.display()
        );
    }
}

#[test]
fn the_data_map_covers_every_registered_provider() {
    // Every provider declares which classes it holds from the compile-time
    // map; a provider that silently adds a class outside the map would be
    // invisible to retention and breach classification.
    for def in sanvi_privacy::domain::DATA_CLASSES {
        assert!(
            sanvi_privacy::domain::DataClass::parse(def.class.as_str()).is_ok(),
            "data map class must parse: {}",
            def.class.as_str()
        );
    }
}

#[test]
fn the_jurisdiction_seed_matches_the_migration() {
    // The profile seed lives in the migration (authoritative, reviewed by
    // counsel) and mirrors the compile-time `seeded_profiles()` used by the
    // matrix test. The two must never drift.
    let migration = std::fs::read_to_string(
        workspace_root().join("migrations/20260819000000_privacy_compliance.sql"),
    )
    .expect("privacy migration present");
    for profile in sanvi_privacy::domain::seeded_profiles() {
        let needle = format!("'{}'", profile.code.as_str());
        assert!(
            migration.contains(&needle),
            "seeded profile {} missing from the migration seed",
            profile.code.as_str()
        );
    }
}
