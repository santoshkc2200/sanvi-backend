use std::borrow::Cow;

use utoipa::{
    OpenApi, PartialSchema, ToSchema,
    openapi::{
        KnownFormat, RefOr,
        schema::{ObjectBuilder, Schema, SchemaFormat, SchemaType, Type},
    },
};

use crate::error::{ApiError, ProblemDetail};

/// Schemas shared by every context's OpenAPI document.
///
/// Each context exports its own `#[derive(OpenApi)]` doc; the composition
/// root merges them and mounts the result at `/openapi.json` plus a Scalar UI
/// at `/scalar`.
#[derive(OpenApi)]
#[openapi(
    components(schemas(ProblemDetail, ApiError)),
    tags(
        (name = "system", description = "System, health and build information")
    )
)]
pub struct BaseApiDoc;

/// The wire schema for every first-party Snowflake entity ID.
///
/// IDs are JSON strings so JavaScript clients never lose precision. The
/// pattern rules out zero, signs, and UUID-shaped legacy values.
pub fn snowflake_id_schema() -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::String))
        .format(Some(SchemaFormat::Custom("snowflake-id".to_string())))
        .pattern(Some("^[1-9][0-9]{0,18}$"))
        .examples([serde_json::json!("873698342314721281")])
}

/// The wire schema for a collection endpoint's `limit` query parameter
/// (TASK-021, FR-1108).
///
/// Every page-size parameter in every context uses this shape via
/// `#[param(schema_with = …)]`: integer, `minimum: 1`, and — the part the
/// bounded-collections gate enforces — a declared `maximum` and `default`, so
/// the contract itself states the bound instead of leaving it to a server-side
/// clamp the client cannot see. Returns the builder so an endpoint can layer
/// its own description on before `.build()`; the schema-level description
/// exists because `schema_with` suppresses utoipa's doc-comment description.
/// Keep `(maximum, default)` in step with the
/// `sanvi_kernel::pagination::clamp_page_size` call serving the same endpoint.
pub fn page_limit_schema(maximum: u64, default: u64) -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::Integer))
        .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int32)))
        .minimum(Some(1i32))
        .maximum(Some(utoipa::Number::from(maximum)))
        .default(Some(serde_json::json!(default)))
        .examples([serde_json::json!(default)])
        .description(Some(format!(
            "Page size, clamped to 1..={maximum}; {default} when absent."
        )))
}

/// Reusable Utoipa value type for first-party Snowflake ID DTO fields and
/// path parameters.
pub struct SnowflakeId;

impl PartialSchema for SnowflakeId {
    fn schema() -> RefOr<Schema> {
        snowflake_id_schema().build().into()
    }
}

impl ToSchema for SnowflakeId {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("SnowflakeId")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_doc_serializes() {
        let json = BaseApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("\"openapi\""));
        assert!(json.contains("ProblemDetail"));
    }

    #[test]
    fn snowflake_id_schema_is_a_decimal_string() {
        let value = serde_json::to_value(snowflake_id_schema().build()).unwrap();
        assert_eq!(value["type"], "string");
        assert_eq!(value["format"], "snowflake-id");
        assert_eq!(value["pattern"], "^[1-9][0-9]{0,18}$");
        assert_eq!(value["examples"][0], "873698342314721281");
    }

    #[test]
    fn page_limit_schema_declares_minimum_maximum_and_default() {
        let value = serde_json::to_value(page_limit_schema(200, 50).build()).unwrap();
        assert_eq!(value["type"], "integer");
        assert_eq!(value["format"], "int32");
        assert_eq!(value["minimum"], 1);
        assert_eq!(value["maximum"], 200);
        assert_eq!(value["default"], 50);
        assert!(
            value["description"]
                .as_str()
                .is_some_and(|description| description.contains("1..=200")),
            "the schema carries the bound in prose: {value}"
        );
    }
}
