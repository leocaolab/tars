//! Carries a JSON Schema for structured output and tool input, with a `strict` hint the
//! adapters map to each vendor's strict mode. It does not validate output against the schema.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::providers::tools::JsonKind;

/// A JSON Schema document and how strictly the provider should hold its output to it.
///
/// The adapters send it as:
/// - **OpenAI**: `response_format = { type: "json_schema", strict, schema }`
/// - **Gemini**: `responseSchema = schema` + `responseMimeType = "application/json"`
/// - **Anthropic**: emulated via a forced `tool_choice`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonSchema {
    /// The schema document. Its root is an object, as every vendor payload (`response_format`,
    /// `responseSchema`, a tool's `input_schema`) requires: only the constructors and
    /// deserialization make one.
    schema: Map<String, Value>,
    /// Whether to ask the provider to enforce the schema while decoding, where it can; when
    /// false the provider may use a loose JSON mode.
    pub strict: bool,
    /// The schema's name, which OpenAI requires on a schema.
    pub name: Option<String>,
}

/// A schema document whose root is not a JSON object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a JSON Schema's root must be an object, got a JSON {0}")]
pub struct SchemaRootError(pub JsonKind);

impl JsonSchema {
    /// Returns a schema the provider is asked to enforce, named `name`.
    pub fn strict(name: impl Into<String>, schema: Map<String, Value>) -> Self {
        Self {
            schema,
            strict: true,
            name: Some(name.into()),
        }
    }

    /// Returns an unnamed schema the provider may hold output to loosely.
    pub fn loose(schema: Map<String, Value>) -> Self {
        Self {
            schema,
            strict: false,
            name: None,
        }
    }

    pub fn schema(&self) -> &Map<String, Value> {
        &self.schema
    }

    /// Returns the schema document as a JSON value, for a payload that embeds it.
    pub fn to_value(&self) -> Value {
        Value::Object(self.schema.clone())
    }
}

/// Returns the root object of a schema document that arrived as any JSON value (from a caller,
/// a file, another language), for [`JsonSchema::strict`] or [`JsonSchema::loose`].
///
/// # Errors
///
/// Returns [`SchemaRootError`] when the root is not an object.
pub fn schema_root(schema: Value) -> Result<Map<String, Value>, SchemaRootError> {
    match schema {
        Value::Object(map) => Ok(map),
        other => Err(SchemaRootError(JsonKind::of(&other))),
    }
}

/// Builds a JSON object from `"key": value` pairs, each value one token tree that `json!` takes
/// (a literal, a `{...}` or `[...]` group, or a parenthesised expression). Unlike `json!({...})`,
/// it returns the `Map` itself, so a schema written as a literal needs no check that its root is
/// an object.
#[macro_export]
macro_rules! json_object {
    ($($key:literal : $value:tt),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map = $crate::__private::serde_json::Map::new();
        $(map.insert(::std::string::String::from($key), $crate::__private::serde_json::json!($value));)*
        map
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_constructor_sets_flag() {
        let s = JsonSchema::strict("greeting", crate::json_object! {"type": "object"});
        assert!(s.strict);
        assert_eq!(s.name.as_deref(), Some("greeting"));
    }

    #[test]
    fn a_json_object_literal_is_the_map_json_would_hold() {
        let n = 3;
        let map = crate::json_object! {
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a"],
            "maxProperties": (n),
        };
        assert_eq!(
            Value::Object(map),
            json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"], "maxProperties": 3})
        );
        assert!(crate::json_object! {}.is_empty());
    }

    #[test]
    fn a_schema_is_made_only_from_an_object_root() {
        assert!(schema_root(json!({"type": "object"})).is_ok());
        assert_eq!(
            schema_root(json!([])).unwrap_err(),
            SchemaRootError(JsonKind::Array)
        );
        assert!(
            serde_json::from_value::<JsonSchema>(json!({"schema": true, "strict": false})).is_err()
        );
        assert!(
            serde_json::from_value::<JsonSchema>(json!({"schema": {}, "strict": false})).is_ok()
        );
    }
}
