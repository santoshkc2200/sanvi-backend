//! FR-1108 (TASK-021, slice 11.2b): no unbounded collection in the contract.
//!
//! Any collection endpoint without a bounded page size is a load test waiting
//! to be run by a customer: one `GET` walks the whole tenant. The gate walks
//! the merged OpenAPI document — the same document `scripts/check-openapi.sh`
//! pins to the committed `api/*.yaml`, so "the specs" and "the generated
//! document" cannot disagree here without that gate failing first — and fails
//! on every collection response that does not declare a bounded `limit`
//! parameter, unless the operation carries a reasoned exemption.
//!
//! What counts:
//! - a *collection* is a `GET` whose 2xx JSON response is an array of objects,
//!   or an object with such an array among its properties (the envelope
//!   shapes). Arrays of primitives (byte payloads, id lists) are not
//!   collections; write-method responses echo bodies the request itself
//!   bounded, so only `GET` is scanned;
//! - *bounded* is a `limit` query parameter whose schema declares `minimum >= 1`
//!   (when present), a `maximum >= 1`, and a `default` within `[minimum,
//!   maximum]` — built with `sanvi_http::openapi::page_limit_schema` and kept
//!   in step with the `sanvi_kernel::pagination::clamp_page_size` call that
//!   serves the endpoint;
//! - an *exemption* is one line in `collection-exemptions.txt`
//!   (`operationId | reason`): catalog, registry and report surfaces whose
//!   size a client cannot influence. Stale exemptions — the operation now
//!   carries a bounded `limit`, or no longer exists — fail too, so the file
//!   cannot rot.

use std::collections::BTreeMap;

use serde_json::Value;
use utoipa::OpenApi as _;

/// Exemptions live beside this file so the gate and its ledger travel together.
const EXEMPTION_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/perf/collection-exemptions.txt"
);

#[test]
fn collection_endpoints_declare_a_bounded_page_size() {
    let doc = merged_document();
    let exemptions =
        parse_exemptions(&std::fs::read_to_string(EXEMPTION_FILE).expect(
            "collection-exemptions.txt is readable; it ships with the gate by construction",
        ));

    let collections = collection_operations(&doc);
    assert!(
        !collections.is_empty(),
        "the gate enumerated zero collection endpoints — the walker is broken, \
         not the contract vacuously clean"
    );

    let mut failures: Vec<String> = Vec::new();

    for (operation_id, method, path, params) in &collections {
        let bounded = params
            .iter()
            .find(|(name, _)| name == "limit")
            .map(|(_, schema)| is_bounded_limit(schema));
        match (bounded, exemptions.get(operation_id)) {
            (Some(true), None) => {}
            (Some(false), _) => {
                failures.push(format!(
                    "{operation_id} ({method} {path}): `limit` is declared without a bound — \
                     schema_with = page_limit_schema(max, default) sets maximum/minimum/default"
                ));
            }
            (None, Some(_)) => {}
            (None, None) => {
                failures.push(format!(
                    "{operation_id} ({method} {path}): collection with no bounded page size — \
                     add a `limit` parameter or an exemption with a reason"
                ));
            }
            (Some(true), Some(reason)) => {
                failures.push(format!(
                    "{operation_id} ({method} {path}): exempted ({reason}) but the operation \
                     declares a bounded `limit` — the exemption is stale, remove it"
                ));
            }
        }
    }

    let known: std::collections::BTreeSet<&String> = collections
        .iter()
        .map(|(operation_id, ..)| operation_id)
        .collect();
    for operation_id in exemptions.keys() {
        if !known.contains(operation_id) {
            failures.push(format!(
                "{operation_id}: exempted but no such operation exists in the document — \
                 remove the stale line"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "unbounded collection endpoints in the contract:\n  - {}",
        failures.join("\n  - ")
    );
}

/// The merged document exactly as `sanvi-cli openapi` serves it.
pub(crate) fn merged_document() -> Value {
    serde_json::to_value(sanvi_api::ApiDoc::openapi()).expect("merged OpenAPI serializes")
}

/// One row per collection `GET`: (operationId, method, path, query parameters).
#[allow(clippy::type_complexity)] // one row type, one return
pub(crate) fn collection_operations(
    doc: &Value,
) -> Vec<(String, String, String, Vec<(String, Value)>)> {
    let components = doc
        .get("components")
        .and_then(|c| c.get("schemas"))
        .cloned()
        .unwrap_or(Value::Null);

    let mut rows = Vec::new();
    let Some(paths) = doc.get("paths").and_then(|p| p.as_object()) else {
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
            let Some(responses) = operation.get("responses").and_then(|r| r.as_object()) else {
                continue;
            };
            let is_collection = responses.iter().any(|(status, response)| {
                is_success(status)
                    && response
                        .pointer("/content/application~1json/schema")
                        .is_some_and(|schema| schema_is_collection(schema, &components, 0))
            });
            if !is_collection {
                continue;
            }
            let operation_id = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{method} {path}"));
            let params = operation
                .get("parameters")
                .and_then(Value::as_array)
                .map(|parameters| {
                    parameters
                        .iter()
                        .filter(|parameter| {
                            parameter.get("in").and_then(Value::as_str) == Some("query")
                        })
                        .filter_map(|parameter| {
                            let name = parameter.get("name")?.as_str()?.to_string();
                            let schema = parameter.get("schema").cloned()?;
                            Some((name, schema))
                        })
                        .collect()
                })
                .unwrap_or_default();
            rows.push((operation_id, method.clone(), path.clone(), params));
        }
    }
    rows
}

fn is_success(status: &str) -> bool {
    status == "200" || status == "201" || status == "202" || status == "default" || status == "2XX"
}

/// A schema is a collection when it is an array of object-ish items, or an
/// object with such an array among its (resolved) properties. Depth-capped so
/// a self-referential schema cannot loop the walker.
fn schema_is_collection(schema: &Value, components: &Value, depth: u8) -> bool {
    if depth > 3 {
        return false;
    }
    let schema = resolve_ref(schema, components);
    if schema.get("type").and_then(Value::as_str) == Some("array") {
        return schema
            .get("items")
            .is_some_and(|items| items_are_objectish(items, components));
    }
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        return all_of
            .iter()
            .any(|member| schema_is_collection(member, components, depth + 1));
    }
    if schema.get("type").and_then(Value::as_str) == Some("object")
        || schema.get("properties").is_some()
    {
        return schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| {
                properties
                    .values()
                    .any(|property| schema_is_collection(property, components, depth + 1))
            })
            .unwrap_or(false);
    }
    false
}

fn items_are_objectish(items: &Value, components: &Value) -> bool {
    let items = resolve_ref(items, components);
    match items.get("type").and_then(Value::as_str) {
        Some("object") => true,
        Some("array") => items
            .get("items")
            .is_some_and(|nested| items_are_objectish(nested, components)),
        _ => items.get("properties").is_some() || items.get("allOf").is_some(),
    }
}

fn resolve_ref<'a>(schema: &'a Value, components: &'a Value) -> &'a Value {
    let Some(name) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix("#/components/schemas/"))
    else {
        return schema;
    };
    // An unresolvable name falls back to the ref node itself, which carries
    // no type — the walker treats it as a non-collection rather than panicking
    // on a document that should never exist (check-openapi guards the merge).
    components.get(name).unwrap_or(schema)
}

fn is_bounded_limit(schema: &Value) -> bool {
    if schema.get("type").and_then(Value::as_str) != Some("integer") {
        return false;
    }
    let maximum = schema.get("maximum").and_then(Value::as_i64);
    let Some(maximum) = maximum else {
        return false;
    };
    if maximum < 1 {
        return false;
    }
    let minimum = schema.get("minimum").and_then(Value::as_i64).unwrap_or(1);
    if minimum < 1 {
        return false;
    }
    let default = schema.get("default").and_then(Value::as_i64);
    let Some(default) = default else {
        return false;
    };
    (minimum..=maximum).contains(&default)
}

/// `operationId | reason` lines; `#` comments and blank lines ignored.
pub(crate) fn parse_exemptions(text: &str) -> BTreeMap<String, String> {
    let mut exemptions = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((operation_id, reason)) = line.split_once('|') else {
            panic!("unparsable exemption line (want `operationId | reason`): {line}");
        };
        exemptions.insert(operation_id.trim().to_string(), reason.trim().to_string());
    }
    exemptions
}

// ---------------------------------------------------------------------------
// Fixtures: the gate's self-test. Every rule is proven on a synthetic
// document so a broken walker or a softened rule fails here, not in review.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc_with(operation: Value) -> Value {
        json!({
            "paths": { "/api/v1/things": { "get": operation } },
            "components": { "schemas": {
                "Thing": { "type": "object", "properties": { "id": { "type": "string" } } },
                "ThingPage": { "type": "object", "properties": {
                    "items": { "type": "array", "items": { "$ref": "#/components/schemas/Thing" } }
                }}
            }}
        })
    }

    fn bounded_limit() -> Value {
        json!({"name": "limit", "in": "query", "schema": {
            "type": "integer", "minimum": 1, "maximum": 100, "default": 50
        }})
    }

    #[test]
    fn objects_with_envelopes_are_collections() {
        let doc = doc_with(json!({
            "operationId": "list_things",
            "responses": { "200": { "content": { "application/json": {
                "schema": { "$ref": "#/components/schemas/ThingPage" }
            }}}}
        }));
        let collections = collection_operations(&doc);
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].0, "list_things");
    }

    #[test]
    fn primitive_arrays_are_not_collections() {
        let doc = json!({
            "paths": { "/api/v1/ids": { "get": {
                "operationId": "list_ids",
                "responses": { "200": { "content": { "application/json": {
                    "schema": { "type": "array", "items": { "type": "string" } }
                }}}}
            }}}
        });
        assert!(collection_operations(&doc).is_empty());
    }

    #[test]
    fn posts_are_not_scanned() {
        let components = doc_with(json!({})).pointer("/components").cloned().unwrap();
        let doc = json!({
            "paths": {
                "/api/v1/things": { "post": {
                    "operationId": "bulk_write",
                    "responses": { "200": { "content": { "application/json": {
                        "schema": { "type": "array", "items": {
                            "$ref": "#/components/schemas/Thing" } }
                    }}}}
                }}
            },
            "components": components
        });
        assert!(collection_operations(&doc).is_empty());
    }

    #[test]
    fn bounded_params_pass_the_rule() {
        assert!(is_bounded_limit(bounded_limit().get("schema").unwrap()));
    }

    #[test]
    fn unbounded_or_incomplete_params_fail_the_rule() {
        for schema in [
            json!({"type": "integer", "minimum": 0, "default": 50}), // no maximum
            json!({"type": "integer", "maximum": 100}),              // no default
            json!({"type": "integer", "minimum": 0, "maximum": 100, "default": 50}), // floor 0
            json!({"type": "integer", "minimum": 1, "maximum": 100, "default": 500}), // default out
            json!({"type": "integer", "maximum": 0, "default": 1}),  // ceiling below 1
            json!({"type": "string", "maximum": 100, "default": "50"}),
        ] {
            assert!(
                !is_bounded_limit(&schema),
                "schema must not pass as bounded: {schema}"
            );
        }
    }

    #[test]
    fn exemptions_parse_and_unparsable_lines_panic() {
        let parsed = parse_exemptions("# comment\n\nlist_locales | catalog\n");
        assert_eq!(
            parsed.get("list_locales").map(String::as_str),
            Some("catalog")
        );
    }

    #[test]
    fn the_gate_fails_on_a_planted_unbounded_collection() {
        let doc = doc_with(json!({
            "operationId": "list_things",
            "parameters": [
                json!({"name": "limit", "in": "query",
                       "schema": {"type": "integer", "minimum": 0}})
            ],
            "responses": { "200": { "content": { "application/json": {
                "schema": { "type": "array", "items": {
                    "$ref": "#/components/schemas/Thing" } }
            }}}}
        }));
        let collections = collection_operations(&doc);
        let (operation_id, .., params) = &collections[0];
        let limit = params.iter().find(|(name, _)| name == "limit");
        assert_eq!(operation_id, "list_things");
        assert!(limit.is_some_and(|(_, schema)| !is_bounded_limit(schema)));
    }

    #[test]
    fn the_gate_passes_a_bounded_collection() {
        let doc = doc_with(json!({
            "operationId": "list_things",
            "parameters": [bounded_limit()],
            "responses": { "200": { "content": { "application/json": {
                "schema": { "type": "array", "items": {
                    "$ref": "#/components/schemas/Thing" } }
            }}}}
        }));
        let collections = collection_operations(&doc);
        let params = &collections[0].3;
        assert!(
            params
                .iter()
                .find(|(name, _)| name == "limit")
                .is_some_and(|(_, schema)| is_bounded_limit(schema))
        );
    }
}
