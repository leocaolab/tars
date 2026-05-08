//! JSON Schema wrapper for structured output and tool input/output.
//!
//! Stays deliberately thin: we don't validate at this layer (that's the
//! Pipeline / Agent's job). We only carry the schema document plus a
//! `strict` hint that maps to OpenAI/Anthropic/Gemini's strict modes.

use serde::{Deserialize, Serialize};

/// A JSON Schema document plus metadata controlling how the Provider
/// adapter should request strict validation.
///
/// Provider-side translation:
/// - **OpenAI**: `response_format = { type: "json_schema", strict, schema }`
/// - **Gemini**: `responseSchema = schema` + `responseMimeType = "application/json"`
/// - **Anthropic**: emulated via a forced `tool_choice` (Doc 01 §9)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonSchema {
    /// The schema document. Must be a JSON object at the root.
    pub schema: serde_json::Value,
    /// If true, ask the provider to enforce structural compliance at
    /// decode time (when supported). When false, the provider is free
    /// to use a "loose" json mode.
    pub strict: bool,
    /// Optional name — useful for OpenAI which wants an identifier on
    /// the schema, and for diagnostics.
    pub name: Option<String>,
}

impl JsonSchema {
    pub fn strict(name: impl Into<String>, schema: serde_json::Value) -> Self {
        Self {
            schema,
            strict: true,
            name: Some(name.into()),
        }
    }

    pub fn loose(schema: serde_json::Value) -> Self {
        Self {
            schema,
            strict: false,
            name: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_constructor_sets_flag() {
        let s = JsonSchema::strict("greeting", json!({"type":"object"}));
        assert!(s.strict);
        assert_eq!(s.name.as_deref(), Some("greeting"));
    }
}
