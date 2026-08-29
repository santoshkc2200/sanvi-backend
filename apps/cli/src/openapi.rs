//! `openapi`: print or verify the merged contract document.
//!
//! The generated document is split per bounded context by the `tag` on each
//! path and compared against the committed `api/{tag}.yaml` specs. CI runs
//! `openapi --check` so the committed specs are always the truth the
//! frontend generates from.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use sanvi_api::ApiDoc;
use utoipa::OpenApi as _;

/// Where per-context specs live.
const API_DIR: &str = "api";

pub fn run(check: bool, write: bool) -> anyhow::Result<()> {
    let doc = ApiDoc::openapi();
    let mut json = serde_json::to_value(&doc).context("failed to serialize openapi document")?;
    annotate_snowflake_ids(&mut json);

    if !check && !write {
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
        return Ok(());
    }

    if write {
        return write_specs(&json);
    }

    let specs = list_specs()?;
    if specs.is_empty() {
        bail!(
            "no committed specs found in {API_DIR}/; run `cargo run -p sanvi-cli -- openapi --write` first"
        );
    }

    let mut drift = Vec::new();
    for (context, path) in specs {
        match check_context(&json, &context, &path) {
            Ok(()) => println!("{context}: OK"),
            Err(err) => drift.push(format!("{context}: {err}")),
        }
    }

    if !drift.is_empty() {
        bail!(
            "OpenAPI drift detected:\n  - {}\n\nRegenerate the committed specs from the annotations.",
            drift.join("\n  - ")
        );
    }
    Ok(())
}

/// Keep generated contracts aligned with the typed ID boundary without
/// repeating Utoipa's schema attributes in every context DTO.
fn annotate_snowflake_ids(document: &mut serde_json::Value) {
    let schema = serde_json::to_value(sanvi_http::openapi::snowflake_id_schema().build())
        .expect("the fixed Snowflake schema serializes");
    annotate_value(document, &schema);
}

fn annotate_value(value: &mut serde_json::Value, snowflake_schema: &serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    if let Some(properties) = object
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        for (name, property) in properties {
            if is_snowflake_field(name) {
                replace_id_schema(property, snowflake_schema);
            } else {
                annotate_value(property, snowflake_schema);
            }
        }
    }

    if let Some(parameters) = object
        .get_mut("parameters")
        .and_then(serde_json::Value::as_array_mut)
    {
        for parameter in parameters {
            let name = parameter.get("name").and_then(serde_json::Value::as_str);
            if name.is_some_and(is_snowflake_field) {
                if let Some(schema) = parameter.get_mut("schema") {
                    *schema = snowflake_schema.clone();
                }
            }
        }
    }

    for nested in object.values_mut() {
        annotate_value(nested, snowflake_schema);
    }
}

fn replace_id_schema(property: &mut serde_json::Value, snowflake_schema: &serde_json::Value) {
    if property.get("type") == Some(&serde_json::Value::String("array".to_string())) {
        if let Some(items) = property.get_mut("items") {
            *items = snowflake_schema.clone();
        }
    } else {
        *property = snowflake_schema.clone();
    }
}

fn is_snowflake_field(name: &str) -> bool {
    matches!(
        name,
        "id" | "after_id"
            | "tenant_id"
            | "user_id"
            | "actor_id"
            | "role_id"
            | "role_ids"
            | "invitation_id"
            | "plan_id"
            | "price_id"
            | "subscription_id"
            | "invoice_id"
            | "connection_id"
            | "checkout_id"
            | "payment_id"
            | "refund_id"
            | "dispute_id"
            | "payout_id"
            | "domain_id"
            | "order_id"
            | "certificate_id"
            | "theme_id"
            | "theme_version_id"
            | "dsr_id"
            | "subject_id"
            | "appeal_id"
            | "authorized_agent_id"
            | "breach_incident_id"
            | "consent_id"
            | "export_artifact_id"
            | "legal_hold_id"
            | "preference_signal_id"
            | "processing_activity_id"
            | "risk_assessment_id"
            | "verification_challenge_id"
            | "campaign_id"
            | "campaign_change_id"
            | "conversion_id"
            | "conversion_event_id"
            | "creative_id"
            | "ad_group_id"
            | "ad_id"
            | "impersonation_grant_id"
            | "approval_id"
    )
}

/// (Re)write api/{tag}.yaml for every tag present in the generated paths.
fn write_specs(full_doc: &serde_json::Value) -> anyhow::Result<()> {
    let dir = std::path::Path::new(API_DIR);
    std::fs::create_dir_all(dir)?;

    let tags = all_tags(full_doc);
    if tags.is_empty() {
        bail!("no tagged paths in the generated document");
    }
    for tag in tags {
        let paths = extract_paths(full_doc, &tag);
        let doc = serde_json::json!({
            "openapi": "3.1.0",
            "info": full_doc.get("info").cloned().unwrap_or(serde_json::Value::Null),
            "paths": paths,
        });
        let yaml = serde_yaml::to_string(&doc).context("failed to serialize yaml")?;
        let path = dir.join(format!("{tag}.yaml"));
        std::fs::write(&path, yaml)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn all_tags(doc: &serde_json::Value) -> Vec<String> {
    let mut tags = std::collections::BTreeSet::new();
    if let Some(paths) = doc.get("paths").and_then(|p| p.as_object()) {
        for item in paths.values() {
            if let Some(item) = item.as_object() {
                for op in item.values().filter_map(|op| op.as_object()) {
                    if let Some(tag_list) = op.get("tags").and_then(|t| t.as_array()) {
                        for tag in tag_list.iter().filter_map(|t| t.as_str()) {
                            tags.insert(tag.to_string());
                        }
                    }
                }
            }
        }
    }
    tags.into_iter().collect()
}

fn list_specs() -> anyhow::Result<Vec<(String, std::path::PathBuf)>> {
    let dir = std::path::Path::new(API_DIR);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut specs = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            let context = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            specs.push((context, path));
        }
    }
    specs.sort();
    Ok(specs)
}

/// Compare the generated paths tagged `context` against the committed spec.
fn check_context(
    full_doc: &serde_json::Value,
    context: &str,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    let committed_text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let committed: serde_json::Value =
        serde_yaml::from_str(&committed_text).context("committed spec is not valid YAML")?;

    let generated_paths = extract_paths(full_doc, context);
    let committed_paths = extract_paths(&committed, context);

    if generated_paths != committed_paths {
        let generated = serde_yaml::to_string(&serde_json::json!({ "paths": generated_paths }))?;
        let committed_yaml =
            serde_yaml::to_string(&serde_json::json!({ "paths": committed_paths }))?;
        bail!(
            "path set differs from {}\n--- generated ---\n{}--- committed ---\n{}",
            path.display(),
            generated,
            committed_yaml
        );
    }
    Ok(())
}

/// All path items whose operations carry the context tag.
fn extract_paths(doc: &serde_json::Value, context: &str) -> BTreeMap<String, serde_json::Value> {
    let mut paths = BTreeMap::new();
    let Some(all_paths) = doc.get("paths").and_then(|p| p.as_object()) else {
        return paths;
    };
    for (path, item) in all_paths {
        let Some(item_map) = item.as_object() else {
            continue;
        };
        let tagged = item_map
            .values()
            .filter_map(|op| op.as_object())
            .filter_map(|op| op.get("tags"))
            .filter_map(|tags| tags.as_array())
            .flatten()
            .any(|tag| tag.as_str() == Some(context));
        if tagged {
            paths.insert(path.clone(), item.clone());
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_paths_filters_by_tag() {
        let doc = json!({
            "paths": {
                "/healthz": { "get": { "tags": ["system"] } },
                "/api/v1/billing/invoices": { "get": { "tags": ["billing"] } },
                "/api/v1/system/version": { "get": { "tags": ["system"] } }
            }
        });
        let system_paths = extract_paths(&doc, "system");
        assert_eq!(system_paths.len(), 2);
        assert!(system_paths.contains_key("/healthz"));
        assert!(!system_paths.contains_key("/api/v1/billing/invoices"));

        let billing_paths = extract_paths(&doc, "billing");
        assert_eq!(billing_paths.len(), 1);
    }

    #[test]
    fn all_tags_collects_unique_tags() {
        let doc = json!({
            "paths": {
                "/a": { "get": { "tags": ["system"] } },
                "/b": { "get": { "tags": ["billing"] }, "post": { "tags": ["system"] } }
            }
        });
        assert_eq!(all_tags(&doc), vec!["billing", "system"]);
    }

    #[test]
    fn snowflake_fields_use_the_shared_decimal_schema() {
        let mut document = json!({
            "components": {
                "schemas": {
                    "TenantView": {
                        "properties": {
                            "id": { "type": "string" },
                            "tenant_id": { "type": "string" },
                            "provider_event_id": { "type": "string" },
                            "role_ids": { "type": "array", "items": { "type": "string" } }
                        }
                    }
                }
            }
        });
        annotate_snowflake_ids(&mut document);
        let properties = &document["components"]["schemas"]["TenantView"]["properties"];
        assert_eq!(properties["id"]["format"], "snowflake-id");
        assert_eq!(properties["tenant_id"]["pattern"], "^[1-9][0-9]{0,18}$");
        assert_eq!(properties["role_ids"]["items"]["format"], "snowflake-id");
        assert_eq!(
            properties["provider_event_id"]["format"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn check_context_passes_when_specs_match_and_fails_on_drift() {
        let dir = std::env::temp_dir().join(format!("sanvi-openapi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("system.yaml");

        let full = json!({
            "paths": {
                "/healthz": { "get": { "tags": ["system"], "responses": {} } }
            }
        });

        // Matching spec passes.
        std::fs::write(
            &path,
            "paths:\n  /healthz:\n    get:\n      tags:\n        - system\n      responses: {}\n",
        )
        .unwrap();
        check_context(&full, "system", &path).expect("matching spec must pass");

        // Drifted spec fails with a diff in the message.
        std::fs::write(
            &path,
            "paths:\n  /readyz:\n    get:\n      tags:\n        - system\n      responses: {}\n",
        )
        .unwrap();
        let err = check_context(&full, "system", &path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("path set differs"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_context_rejects_invalid_yaml() {
        let dir = std::env::temp_dir().join(format!("sanvi-openapi-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("system.yaml");
        std::fs::write(&path, "not: [valid").unwrap();
        let full = json!({ "paths": {} });
        assert!(check_context(&full, "system", &path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_specs_returns_empty_for_missing_dir() {
        assert!(
            list_specs().unwrap_or_default().is_empty() || std::path::Path::new(API_DIR).is_dir()
        );
    }
}
