//! TASK-024 step 7 (slice 11.5f): the secret-bearing-field enumeration gate.
//!
//! The test enumerates secret-shaped struct fields **from the source tree**,
//! not from a hand-written list, and fails when a new one appears without
//! redaction. A field is safe exactly when one of these holds:
//!
//! 1. **Type-wrapped** — its type is `Redacted<..>` (or the advertising
//!    context's opaque `SecretRef`), both of which render `<redacted>` /
//!    `[REDACTED]` through Debug, Display and Serialize by construction.
//! 2. **Classified** — it is listed in `tests/secret-exemptions.txt` with a
//!    class whose mechanical rule the field's *owning struct* satisfies:
//!
//! | class | rule on the owning struct's derives / impls |
//! | --- | --- |
//! | `config-carrier`, `runtime-carrier`, `cli-arg` | no `Debug`, no `Serialize` |
//! | `wire-in`, `capability-input` | no `Serialize` |
//! | `wire-out`, `capability-delivery` | no `Debug` |
//! | `manual-redaction` | a same-file manual `Debug` for the struct (or the field's own type) that renders a redaction marker, **and** serde safety: the field is `skip_serializing`, or a manual redacting `Serialize` exists, or the struct does not derive `Serialize` |
//! | `public-by-design`, `ciphertext`, `not-a-secret` | no mechanical rule; the recorded reason is the argument |
//!
//! Anything else — a new `token: String`, a `password` field, an exemption
//! whose class rule its struct violates — is a SECRET FINDING and fails the
//! build. A stale exemption entry (nothing matches it) also fails, so the
//! file cannot rot.
//!
//! `scripts/check-secret-scan.sh` is the gate wrapper; `--self-test` runs
//! the fixture suite, which plants an unclassified field, a rule-violating
//! exemption and a wrapped control, and asserts each is reported by name.
//!
//! Scope limits, stated rather than hidden: the scanner reads struct/enum
//! *declaration bodies* (so function parameters and struct-literal
//! constructions never match), skips `#[cfg(test)]` modules and any path
//! under a `tests/` directory, and recognises single-line `#[derive(..)]`
//! attributes — the shapes this workspace actually uses. Macro-generated
//! declarations are invisible to it; that residual gap is accepted because
//! the workspace generates no secret-bearing structs in macros.

use std::fs;
use std::path::{Path, PathBuf};

/// Substrings that make a field name secret-shaped regardless of suffix
/// (e.g. `secret`, `client_secret`, `db_password`, `stripe_api_key`,
/// `signing_keys`, `credentials`, `passphrase_verifier`).
const NAME_CONTAINS: &[&str] = &[
    "secret",
    "password",
    "api_key",
    "private_key",
    "credential",
    "passphrase",
    "signing_key",
];

/// Exact names / suffixes that are secret-shaped on their own (`token`,
/// `refresh_token`, `kek`, `wrapped_dek`, …). Longer, non-secret meanings
/// (`token_expires_at`, `token_hash`, `next_page_token`) do not match and
/// are handled by the `not-a-secret` class only when they do.
const NAME_EXACT_OR_SUFFIX: &[&str] = &["token", "kek", "dek"];

fn name_is_secret_shaped(name: &str) -> bool {
    if NAME_CONTAINS.iter().any(|k| name.contains(k)) {
        return true;
    }
    NAME_EXACT_OR_SUFFIX
        .iter()
        .any(|s| name == *s || name.ends_with(&format!("_{s}")))
}

#[derive(Debug)]
struct Finding {
    path: String,
    line: usize,
    item: String,
    field: String,
    ty: String,
    derives_debug: bool,
    derives_serialize: bool,
    manual_debug_redacts: bool,
    manual_serde_redacts: bool,
    field_skip_serializing: bool,
}

impl Finding {
    /// The base type name with common wrappers stripped — used to find a
    /// manual redacting impl on the field's own type (`Option<Credentials>`
    /// → `Credentials`).
    fn field_base_type(&self) -> Option<&str> {
        let mut ty = self.ty.trim();
        for wrapper in ["Option<", "Vec<", "Arc<", "Box<", "Redacted<"] {
            if let Some(rest) = ty.strip_prefix(wrapper) {
                ty = rest.trim();
            }
        }
        ty = ty.trim_end_matches('>');
        ty.split("::")
            .last()
            .filter(|s| s.chars().next().is_some_and(char::is_uppercase))
            .map(|s| s.trim())
    }

    fn type_is_wrapped(&self) -> bool {
        self.ty.contains("Redacted") || self.ty.contains("SecretRef")
    }

    /// serde safety for the `manual-redaction` class.
    fn serde_safe(&self) -> bool {
        !self.derives_serialize
            || self.field_skip_serializing
            || self.manual_serde_redacts
            || self
                .field_base_type()
                .is_some_and(|_| self.manual_serde_redacts)
    }
}

/// One parsed line, comment-stripped.
struct SourceLine {
    text: String,
    cfg_test: bool,
}

fn load_source(path: &Path) -> Vec<SourceLine> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    let mut in_block_comment = false;
    for line in raw.lines() {
        let mut text = line.to_string();
        if in_block_comment {
            if let Some(end) = text.find("*/") {
                text = text[end + 2..].to_string();
                in_block_comment = false;
            } else {
                out.push(SourceLine {
                    text: String::new(),
                    cfg_test: false,
                });
                continue;
            }
        }
        while let Some(start) = text.find("/*") {
            if let Some(end_rel) = text[start + 2..].find("*/") {
                let end = start + 2 + end_rel + 2;
                text = format!("{}{}", &text[..start], &text[end..]);
            } else {
                text = text[..start].to_string();
                in_block_comment = true;
                break;
            }
        }
        if let Some(idx) = text.find("//") {
            text.truncate(idx);
        }
        out.push(SourceLine {
            text,
            cfg_test: line.contains("#[cfg(test)]"),
        });
    }
    out
}

/// Mark the brace range of every `#[cfg(test)] mod … { … }` block.
fn cfg_test_skip_mask(lines: &[SourceLine]) -> Vec<bool> {
    let mut skip = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].cfg_test {
            // find the `mod NAME {` that follows (attributes may interleave)
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].text.trim();
                if t.starts_with("#[") || t.is_empty() {
                    j += 1;
                    continue;
                }
                break;
            }
            if j < lines.len() && lines[j].text.trim_start().starts_with("mod ") {
                let mut k = j;
                while k < lines.len() && !lines[k].text.contains('{') {
                    k += 1;
                }
                if k < lines.len() {
                    let mut depth = 0usize;
                    let mut m = k;
                    while m < lines.len() {
                        depth += lines[m].text.matches('{').count();
                        depth = depth.saturating_sub(lines[m].text.matches('}').count());
                        skip[i..=m].fill(true);
                        if depth == 0 {
                            break;
                        }
                        m += 1;
                    }
                    i = m + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    skip
}

/// Which structs have a same-file manual `impl … Debug/Serialize for X`
/// whose body renders a redaction marker.
fn manual_redacting_impls(lines: &[SourceLine], skip: &[bool]) -> (Vec<String>, Vec<String>) {
    let mut debug_redacts = Vec::new();
    let mut serialize_redacts = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if skip[i] {
            i += 1;
            continue;
        }
        let t = lines[i].text.trim_start();
        let is_debug = t.starts_with("impl") && t.contains("Debug for ");
        let is_serialize = t.starts_with("impl") && t.contains("Serialize for ");
        if is_debug || is_serialize {
            let name = t
                .split(" for ")
                .nth(1)
                .map(|rest| {
                    rest.trim()
                        .split(|c: char| c.is_whitespace() || c == '{' || c == '<')
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            let mut depth = 0usize;
            let mut started = false;
            let mut body = String::new();
            let mut k = i;
            while k < lines.len() {
                if started && skip[k] {
                    break;
                }
                depth += lines[k].text.matches('{').count();
                depth = depth.saturating_sub(lines[k].text.matches('}').count());
                if lines[k].text.contains('{') {
                    started = true;
                }
                body.push_str(&lines[k].text);
                if started && depth == 0 {
                    let redacts = body.contains("edacted") || body.contains("EDACTED");
                    if redacts && !name.is_empty() {
                        if is_debug {
                            debug_redacts.push(name.clone());
                        } else {
                            serialize_redacts.push(name.clone());
                        }
                    }
                    i = k + 1;
                    break;
                }
                k += 1;
            }
        }
        i += 1;
    }
    (debug_redacts, serialize_redacts)
}

fn scan_file(root: &Path, rel: &str, findings: &mut Vec<Finding>) {
    let path = root.join(rel);
    let lines = load_source(&path);
    let skip = cfg_test_skip_mask(&lines);
    let (manual_debug, manual_serialize) = manual_redacting_impls(&lines, &skip);

    let mut depth = 0usize;
    // (item name, brace depth *outside* its body, derives text)
    let mut open_items: Vec<(String, usize, String)> = Vec::new();
    // struct/enum declaration seen, body `{` not yet reached
    let mut pending: Option<(String, String)> = None;
    let mut attr_buf: Vec<String> = Vec::new();
    // attributes accumulated just above the current line while inside an
    // item body (e.g. a field's `#[serde(skip_serializing)]`)
    let mut field_attrs: Vec<String> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if skip[idx] {
            depth += line.text.matches('{').count();
            depth = depth.saturating_sub(line.text.matches('}').count());
            continue;
        }
        let text = line.text.as_str();
        let trimmed = text.trim();

        if trimmed.starts_with("#[") {
            attr_buf.push(trimmed.to_string());
            field_attrs.push(trimmed.to_string());
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }

        // Item declaration: struct/enum NAME (never `impl`, `trait`, `fn`).
        let decl = trimmed
            .trim_start_matches("pub")
            .trim_start_matches("(crate)")
            .trim_start();
        if let Some(rest) = decl
            .strip_prefix("struct ")
            .or_else(|| decl.strip_prefix("enum "))
        {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == '<' || c == '(' || c == '{' || c == ';')
                .next()
                .unwrap_or("")
                .to_string();
            if !name.is_empty() {
                if trimmed.contains('{') {
                    open_items.push((name, depth, attr_buf.join(" ")));
                } else if !trimmed.ends_with(';') {
                    // A trailing `;` on the declaration line is a unit or
                    // tuple struct: no named body ever follows, so leaving
                    // `pending` set would adopt the next `{` — typically an
                    // `impl` block — as the struct body and misread function
                    // parameters as fields.
                    pending = Some((name, attr_buf.join(" ")));
                }
            }
            attr_buf.clear();
            field_attrs.clear();
        } else if let Some((name, derives)) = pending.take() {
            // Continuation between `struct X` and its `{`: only generics /
            // where-clauses are legitimate. A `;` ends a unit or tuple
            // struct, whose fields are positional and never name-matched.
            if trimmed.contains('{') {
                open_items.push((name, depth, derives));
            } else if !trimmed.contains(';') {
                pending = Some((name, derives));
            }
        } else {
            attr_buf.clear();
        }

        // Inside an item body, a field line sits exactly one brace level
        // below the declaration's own level.
        if let Some(top) = open_items.last()
            && depth == top.1 + 1
            && let Some((field, ty)) = parse_field(trimmed)
            && name_is_secret_shaped(&field)
        {
            let item_name = top.0.clone();
            let base = strip_wrappers(&ty);
            findings.push(Finding {
                path: rel.to_string(),
                line: idx + 1,
                item: item_name.clone(),
                field,
                ty: ty.clone(),
                derives_debug: top.2.contains("Debug"),
                derives_serialize: top.2.contains("Serialize"),
                manual_debug_redacts: manual_debug.iter().any(|n| n == &item_name)
                    || base
                        .as_deref()
                        .is_some_and(|b| manual_debug.iter().any(|n| n == b)),
                manual_serde_redacts: manual_serialize.iter().any(|n| n == &item_name)
                    || base
                        .as_deref()
                        .is_some_and(|b| manual_serialize.iter().any(|n| n == b)),
                field_skip_serializing: field_attrs.iter().any(|a| a.contains("skip_serializing")),
            });
        }

        depth += text.matches('{').count();
        depth = depth.saturating_sub(text.matches('}').count());
        field_attrs.clear();
        while let Some(top) = open_items.last() {
            if depth <= top.1 {
                open_items.pop();
            } else {
                break;
            }
        }
    }
}

fn strip_wrappers(ty: &str) -> Option<String> {
    let mut t = ty.trim().trim_end_matches(',').trim();
    for wrapper in ["Option<", "Vec<", "Arc<", "Box<"] {
        if let Some(rest) = t.strip_prefix(wrapper) {
            t = rest.trim();
        }
    }
    t = t.trim_end_matches('>');
    t.split("::")
        .last()
        .map(|s| s.trim().to_string())
        .filter(|s| s.chars().next().is_some_and(char::is_uppercase))
}

/// `pub(crate)? name: Type,` inside a declaration body.
fn parse_field(trimmed: &str) -> Option<(String, String)> {
    let mut body = trimmed;
    let mut vis_len = 0;
    for vis in ["pub(crate) ", "pub(super) ", "pub(in crate) ", "pub "] {
        if let Some(rest) = body.strip_prefix(vis) {
            vis_len += vis.len();
            body = rest;
            break;
        }
    }
    let _ = vis_len;
    let mut parts = body.splitn(2, ':');
    let name = parts.next()?.trim();
    let ty = parts.next()?.trim().trim_end_matches(',').trim();
    if name.is_empty()
        || ty.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        || !name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return None;
    }
    // Reject lines that are clearly not field declarations.
    if name.contains('.') || ty.starts_with('=') || name.contains('(') {
        return None;
    }
    Some((name.to_string(), ty.to_string()))
}

fn workspace_root() -> PathBuf {
    if let Ok(root) = std::env::var("SANVI_SECRET_SCAN_ROOT") {
        return PathBuf::from(root);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn rust_sources(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for base in ["crates", "apps"] {
        let mut stack = vec![root.join(base)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if path.is_dir() {
                    // `tests` trees are integration tests, `target` is build
                    // output; everything else is walked, with files filtered
                    // by the `/src/` path check below.
                    if name != "target" && name != "tests" {
                        stack.push(path);
                    }
                } else if name.ends_with(".rs") && path.to_string_lossy().contains("/src/") {
                    out.push(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .to_string(),
                    );
                }
            }
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Exemptions
// ---------------------------------------------------------------------------

struct Exemption {
    class: String,
    path: String,
    item: String,
    field: String,
    reason: String,
}

fn parse_exemptions(path: &Path) -> Vec<Exemption> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, '|').map(str::trim).collect();
        if parts.len() == 5 {
            out.push(Exemption {
                class: parts[0].to_string(),
                path: parts[1].to_string(),
                item: parts[2].to_string(),
                field: parts[3].to_string(),
                reason: parts[4].to_string(),
            });
        }
    }
    out
}

/// The mechanical rule for an exemption class, evaluated against the
/// finding. `None` = no mechanical rule (reason-only class).
fn class_rule_satisfied(class: &str, f: &Finding) -> Option<bool> {
    match class {
        "config-carrier" | "runtime-carrier" | "cli-arg" => {
            Some(!f.derives_debug && !f.derives_serialize)
        }
        "wire-in" | "capability-input" => Some(!f.derives_serialize),
        "wire-out" | "capability-delivery" => Some(!f.derives_debug),
        "manual-redaction" => Some(f.manual_debug_redacts && f.serde_safe()),
        "public-by-design" | "ciphertext" | "not-a-secret" => None,
        _ => Some(false), // unknown class: always a finding
    }
}

fn run_gate(root: &Path, exemptions_path: &Path) -> (Vec<String>, usize) {
    let exemptions = parse_exemptions(exemptions_path);
    let mut findings = Vec::new();
    for rel in rust_sources(root) {
        scan_file(root, &rel, &mut findings);
    }

    let mut errors = Vec::new();
    let mut used = vec![false; exemptions.len()];
    for f in &findings {
        if f.type_is_wrapped() {
            continue;
        }
        match exemptions
            .iter()
            .position(|e| e.path == f.path && e.item == f.item && e.field == f.field)
        {
            None => errors.push(format!(
                "SECRET FINDING: {}:{} {}.{} (`{}`) is not classified — wrap it in Redacted<..> / SecretRef, or add an entry to crates/platform/telemetry/tests/secret-exemptions.txt",
                f.path, f.line, f.item, f.field, f.ty
            )),
            Some(idx) => {
                used[idx] = true;
                match class_rule_satisfied(&exemptions[idx].class, f) {
                    Some(true) => {}
                    Some(false) => errors.push(format!(
                        "SECRET FINDING: {}:{} {}.{} is exempted as `{}` but violates the class rule (derives Debug: {}, derives Serialize: {}, manual redacting Debug: {}, skip_serializing: {}) — fix the struct or re-classify; recorded reason: \"{}\"",
                        f.path, f.line, f.item, f.field, exemptions[idx].class,
                        f.derives_debug, f.derives_serialize, f.manual_debug_redacts,
                        f.field_skip_serializing, exemptions[idx].reason
                    )),
                    None => {}
                }
            }
        }
    }
    for (idx, e) in exemptions.iter().enumerate() {
        if !used[idx] {
            errors.push(format!(
                "STALE EXEMPTION: {} | {} | {} | {} matches no secret-shaped field in the tree — remove or update it",
                e.path, e.item, e.field, e.class
            ));
        }
    }
    (errors, findings.len())
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// The floor is the scanner's vacuity guard: a parser regression that finds
/// zero (or implausibly few) fields must fail loudly rather than pass green.
/// Update it *upward* only, when the tree legitimately grows.
#[test]
fn secret_bearing_fields_are_fully_classified() {
    let root = workspace_root();
    let (errors, total) = run_gate(
        &root,
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/secret-exemptions.txt"),
    );
    assert!(
        total >= 25,
        "scanner vacuity guard: only {total} secret-shaped fields found across the tree — the parser is broken, not the tree clean"
    );
    assert!(
        errors.is_empty(),
        "{} error(s):\n{}",
        errors.len(),
        errors.join("\n")
    );
}

/// The self-test half: the fixture tree plants the three outcomes the gate
/// must distinguish — an unclassified field, an exemption whose class rule
/// its struct violates, and a properly wrapped control that must stay
/// silent. `scripts/check-secret-scan.sh --self-test` expects this test to
/// fail against the *clean-tree* gate configuration pointing at the fixture.
#[test]
fn planted_fixture_violations_are_detected_by_name() {
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/secret-scan");
    let (errors, total) = run_gate(&fixture_root, &fixture_root.join("exemptions.txt"));
    assert!(
        total >= 4,
        "fixture scan found {total} fields — fixture broken"
    );
    let joined = errors.join("\n");
    for expected in [
        "SECRET FINDING: crates/fake/src/unclassified.rs:5 RogueConfig.api_key (`String`) is not classified",
        "SECRET FINDING: crates/fake/src/violating.rs:6 LeakyCarrier.client_secret is exempted as `config-carrier` but violates the class rule",
        "STALE EXEMPTION: crates/fake/src/missing.rs | Ghost | api_key",
    ] {
        assert!(
            joined.contains(expected),
            "expected the fixture to report `{expected}`; got:\n{joined}"
        );
    }
    // The clean controls must be completely silent.
    assert!(
        !joined.contains("wrapped.rs"),
        "the Redacted-wrapped control must not be reported:\n{joined}"
    );
    assert!(
        !joined.contains("exempted.rs"),
        "the clean exemption must pass its class rule:\n{joined}"
    );
}
