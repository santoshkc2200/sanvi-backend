//! Collection ordering (TASK-027, FR-1127).
//!
//! Two layers, one contract: **every collection endpoint declares a
//! deterministic ordering in its spec, ending in the row's unique id**, and
//! the enforcement is real — rows with identical sort keys come back in the
//! declared order, stable across repeated fetches and across page
//! boundaries.
//!
//! *The spec gate* walks the merged OpenAPI document — the same walker the
//! TASK-021 bounded-collections gate uses, so the two gates see the same
//! surface — and fails on any bounded collection whose `summary` or
//! `description` does not carry a machine-parsable clause of the form
//! `Ordered by <key> <ASC|DESC>[, <key> <ASC|DESC>].` whose final key is
//! the unique tie-breaker (`id`; `seq` for the audit log, whose chain
//! position is the row's identity). A handler that drops the clause, or
//! declares a tie-break-less ordering, fails the build.
//!
//! The live probe that served `GET /api/v1/tenant/payments` against seeded
//! ties moved with Payments to the standalone service.

use serde_json::Value;
use utoipa::OpenApi as _;

// ---------------------------------------------------------------------------
// Spec gate
// ---------------------------------------------------------------------------

/// `<key> <ASC|DESC>` pairs, comma-separated, period-terminated.
fn parse_ordering_clause(text: &str) -> Option<Vec<(String, bool)>> {
    let start = text.find("Ordered by ")?;
    let clause = &text[start + "Ordered by ".len()..];
    let end = clause.find('.')?;
    let clause = &clause[..end];
    let mut keys = Vec::new();
    for part in clause.split(", ") {
        let (key, direction) = part.rsplit_once(' ')?;
        if !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            || key.is_empty()
        {
            return None;
        }
        match direction {
            "ASC" => keys.push((key.to_string(), true)),
            "DESC" => keys.push((key.to_string(), false)),
            _ => return None,
        }
    }
    (!keys.is_empty()).then_some(keys)
}

fn operation_text(operation: &Value) -> String {
    [
        operation
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or(""),
        operation
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(""),
    ]
    .join("\n")
}

#[test]
fn every_bounded_collection_declares_an_ordering_tie_broken_by_id() {
    let doc = serde_json::to_value(sanvi_api::ApiDoc::openapi()).expect("merged doc serializes");
    let collections = bounded_collections(&doc);
    assert!(
        !collections.is_empty(),
        "the gate enumerated zero bounded collections — the walker is broken, \
         not the contract vacuously clean"
    );

    let mut failures = Vec::new();
    for (operation_id, path, operation) in &collections {
        let declared = parse_ordering_clause(&operation_text(operation));
        let Some(keys) = declared else {
            failures.push(format!(
                "{operation_id} ({path}): no parseable `Ordered by … .` clause in the summary or \
                 description — declare the deterministic ordering the handler enforces"
            ));
            continue;
        };
        let (last_key, _) = keys.last().expect("non-empty by construction");
        let unique_key = unique_tie_breaker(operation_id, last_key);
        match unique_key {
            Ok(()) => {}
            Err(reason) => failures.push(format!(
                "{operation_id} ({path}): ordering `{last_key}` is not tie-broken by a unique key — \
                 {reason}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "collection endpoints without a specified deterministic ordering:\n  - {}",
        failures.join("\n  - ")
    );
}

/// The unique key a collection's ordering must end in. `id` everywhere,
/// with documented exceptions where the row's identity is named
/// differently: the audit log's chain position `seq` *is* the entry id,
/// a membership row is keyed by `(tenant_id, user_id)` — it has no other
/// id — and a stock level row is keyed by `(location_id, variant_id)`,
/// with the location fixed by the query, so `variant_id` is the identity.
const NON_ID_UNIQUE_KEYS: &[(&str, &str)] = &[
    ("list_audit", "seq"),
    ("list_members", "user_id"),
    ("ecommerce_list_stock", "variant_id"),
];

fn unique_tie_breaker(operation_id: &str, last_key: &str) -> Result<(), String> {
    if last_key == "id" {
        return Ok(());
    }
    match NON_ID_UNIQUE_KEYS
        .iter()
        .find(|(op, key)| *op == operation_id && *key == last_key)
    {
        Some(_) => Ok(()),
        None => Err(format!(
            "end the clause at `id` (exceptions: {})",
            NON_ID_UNIQUE_KEYS
                .iter()
                .map(|(op, key)| format!("{op} → {key}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Collection GETs that declare a bounded `limit` — the TASK-021 non-exempt
/// set, re-derived the same way so the two gates cannot disagree.
fn bounded_collections(doc: &Value) -> Vec<(String, String, Value)> {
    let mut rows = Vec::new();
    let Some(paths) = doc.get("paths").and_then(Value::as_object) else {
        return rows;
    };
    for (path, item) in paths {
        let Some(operations) = item.as_object() else {
            continue;
        };
        for (method, operation) in operations {
            if method != "get" || !operation.is_object() {
                continue;
            }
            let bounded_limit = operation
                .get("parameters")
                .and_then(Value::as_array)
                .map(|params| {
                    params.iter().any(|parameter| {
                        parameter.get("name").and_then(Value::as_str) == Some("limit")
                            && parameter
                                .pointer("/schema/maximum")
                                .and_then(Value::as_i64)
                                .is_some_and(|max| max >= 1)
                            && parameter
                                .pointer("/schema/default")
                                .and_then(Value::as_i64)
                                .is_some()
                    })
                })
                .unwrap_or(false);
            if !bounded_limit {
                continue;
            }
            let operation_id = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("get {path}"));
            rows.push((operation_id, path.clone(), operation.clone()));
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// Spec-gate self-tests
// ---------------------------------------------------------------------------

#[cfg(test)]
fn gate_on(operation: serde_json::Value) -> Result<(), String> {
    let keys = parse_ordering_clause(&operation_text(&operation));
    let operation_id = operation
        .get("operationId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match keys {
        None => Err("no clause".to_string()),
        Some(keys) => {
            let (last, _) = keys.last().expect("non-empty");
            unique_tie_breaker(operation_id, last)
        }
    }
}

#[test]
fn the_spec_gate_fails_on_a_missing_or_unanchored_clause() {
    let doc = serde_json::json!({
        "summary": "List things.",
        "description": "A fine list."
    });
    assert!(gate_on(doc).is_err(), "no clause must fail");

    let unanchored = serde_json::json!({
        "summary": "List things. Ordered by created_at DESC."
    });
    assert!(
        gate_on(unanchored).is_err(),
        "a tie-break-less ordering must fail"
    );

    let good = serde_json::json!({
        "summary": "List things.",
        "description": "Ordered by created_at DESC, id DESC."
    });
    assert!(gate_on(good).is_ok());

    let audit = serde_json::json!({
        "operationId": "list_audit",
        "summary": "The audit trail.",
        "description": "Ordered by seq DESC."
    });
    assert!(gate_on(audit).is_ok(), "seq is the audit row's id");
    let memberships = serde_json::json!({
        "operationId": "list_members",
        "summary": "The tenant's members.",
        "description": "Ordered by created_at ASC, user_id ASC."
    });
    assert!(
        gate_on(memberships).is_ok(),
        "user_id is a membership row's identity"
    );
}
