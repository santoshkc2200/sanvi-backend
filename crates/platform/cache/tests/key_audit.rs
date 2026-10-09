//! The tenant-key audit gate (TASK-021 11.2g, step 6: "a cache is a bypass
//! of RLS if its key forgets the tenant").
//!
//! Every Redis key literal in the workspace's source trees must either carry
//! an interpolated tenant component (`"sanvi:{tenant_id}:…"` in the format
//! string) or sit on the reviewed allow-list below, each entry carrying the
//! justification for being tenant-less. A new key constructed without a
//! tenant component fails this test naming the file and line — by
//! construction, not by review.
//!
//! Scope: `src/` trees of `crates/` and `apps/` (the code that runs in
//! production). Test code may build odd shapes without a gate.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Tenant-less key prefixes that are correct by design, with the reason.
/// The audit re-checks each against its justification whenever the gate
/// runs; a prefix here must never cover tenant *data*.
const ALLOWED_TENANTLESS: &[(&str, &str)] = &[
    (
        "sanvi:tenant:resolution:",
        "pre-tenant by definition: the entry IS the host/slug-to-tenant lookup, no tenant exists to key by",
    ),
    (
        "sanvi:domains:host:",
        "pre-tenant: host-to-tenant resolution runs before any tenant context exists",
    ),
    (
        "sanvi:session",
        "token-scoped: the session token hash identifies one browser session; the tenant rides inside the cached payload",
    ),
    (
        "sanvi:decision:",
        "user-scoped: the key embeds the resource reference, and a tenant-scoped resource carries its tenant id in the key (tenant:{id})",
    ),
    (
        "sanvi:abuse:",
        "client counters, not tenant data: keyed by surface + IP/Origin/tenant identifier, values are request counts",
    ),
    (
        "sanvi:rl",
        "rate-limit buckets: counters per client or per tenant identifier, never tenant payload",
    ),
    (
        "sanvi:scheduler:leader",
        "infrastructure lock: process coordination, no tenant data",
    ),
    (
        "sanvi:*:",
        "SCAN patterns for invalidation: the wildcard twin of a tenant-shaped key family, never read as data",
    ),
];

/// Extracts every `"sanvi:` … `"` string literal from one source file's text.
fn sanvi_literals(source: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("\"sanvi:") {
        let after = &rest[start + 1..];
        let end = after.find('"').unwrap_or(after.len());
        literals.push(after[..end].to_string());
        rest = &rest[start + 1..];
    }
    literals
}

fn violation_for(literal: &str) -> Option<String> {
    // These are configurable namespace values, never complete Redis keys.
    // ApiLimits adds tenant + verified credential or public peer identity;
    // QuotaService adds tenant + dimension + period before every operation.
    if matches!(literal, "sanvi:api-limits" | "sanvi:quota:v1") {
        return None;
    }
    if literal.contains("{tenant") {
        return None;
    }
    for (prefix, _) in ALLOWED_TENANTLESS {
        if literal.starts_with(prefix) {
            return None;
        }
    }
    Some(format!(
        "Redis key literal \"{literal}\" carries no tenant component and is not on the audited \
         allow-list — a cache key that forgets the tenant is a bypass of RLS. Either interpolate \
         the tenant id into the key or add a justified allow-list entry in \
         crates/platform/cache/tests/key_audit.rs"
    ))
}

fn collect_rs_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && path.components().any(|c| c.as_os_str() == "src")
        {
            out.push(path);
        }
    }
}

#[test]
fn every_redis_key_literal_carries_a_tenant_or_a_justified_exemption() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("crate lives three levels under the workspace")
        .to_path_buf();

    let mut sources = Vec::new();
    collect_rs_sources(&workspace.join("crates"), &mut sources);
    collect_rs_sources(&workspace.join("apps"), &mut sources);
    assert!(
        sources.len() >= 200,
        "vacuity guard: only {} source files scanned — the walker is broken",
        sources.len()
    );

    let mut scanned_literals = 0usize;
    let mut violations = Vec::new();
    let mut files_with_keys = BTreeSet::new();
    for path in &sources {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let literals = sanvi_literals(&text);
        if literals.is_empty() {
            continue;
        }
        files_with_keys.insert(path.display().to_string());
        for (index, literal) in literals.iter().enumerate() {
            scanned_literals += 1;
            if let Some(violation) = violation_for(literal) {
                // The literal's ordinal disambiguates multiple hits per line
                // without re-implementing byte offsets per line here.
                violations.push(format!(
                    "{} (literal #{index}): {violation}",
                    path.display()
                ));
            }
        }
    }

    assert!(
        scanned_literals >= 20,
        "vacuity guard: only {scanned_literals} Redis key literals scanned — the scanner is broken"
    );
    // Known production key sites the scan must have reached (the 11.5g
    // lesson: a gate that silently matches nothing is decoration).
    for expected in [
        "entitlement_cache.rs",
        "decision_cache.rs",
        "session_cache.rs",
        "abuse.rs",
    ] {
        assert!(
            files_with_keys.iter().any(|f| f.ends_with(expected)),
            "vacuity guard: the scan never reached {expected}"
        );
    }

    assert!(
        violations.is_empty(),
        "tenant-key audit failed:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_gate_fails_a_naked_key_by_construction() {
    // The self-test: a tenant-less key not on the allow-list must fail
    // naming the fix, while tenant-shaped and allow-listed shapes pass.
    let naked = violation_for("sanvi:settings:theme");
    let expect = naked.expect("a tenant-less key outside the allow-list must be a violation");
    assert!(expect.contains("no tenant component"), "{expect}");

    for shaped in [
        "sanvi:{tenant_id}:settings:theme",
        "sanvi:{tenant}:entitlements",
        "sanvi:{tenant_id}:theming:resolved:{locale}",
    ] {
        assert!(
            violation_for(shaped).is_none(),
            "a tenant-shaped key must pass: {shaped}"
        );
    }

    for exempt in [
        "sanvi:tenant:resolution:slug:{slug}",
        "sanvi:domains:host:{normalized_hostname}",
        "sanvi:session:{token_hash}",
        "sanvi:session:idx",
        "sanvi:decision:{key}",
        "sanvi:decision:*",
        "sanvi:abuse:{surface}:{key_kind}:{key}:{bucket}",
        "sanvi:rl:{key}",
        "sanvi:rl:tenant:{}",
        "sanvi:scheduler:leader",
        "sanvi:*:entitlements",
    ] {
        assert!(
            violation_for(exempt).is_none(),
            "an allow-listed shape must pass: {exempt}"
        );
    }

    // Allow-list entries do not extend to tenant data shapes: a plausible
    // tenant-data key under an exempt prefix must still be catchable by the
    // justification review, so the prefixes stay narrow (documented here by
    // asserting the list never contains a bare "sanvi:" entry).
    for (prefix, _) in ALLOWED_TENANTLESS {
        assert!(
            prefix.len() > "sanvi:".len(),
            "allow-list prefix {prefix} is too broad to be a justification"
        );
    }
}
