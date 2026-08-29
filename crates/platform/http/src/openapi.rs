use std::borrow::Cow;

use utoipa::{
    OpenApi, PartialSchema, ToSchema,
    openapi::{
        RefOr,
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
}
