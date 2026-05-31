//! Tool calling — the Provider-layer view.
//!
//! This is *not* the same as Doc 05's `Tool` trait. Doc 05's `Tool` is
//! a callable thing the Runtime invokes. This module is just the
//! request/response shape that the Provider exchanges with the LLM:
//!
//! - [`ToolSpec`] — definition we send *to* the LLM in `request.tools`
//! - [`ToolCall`] — call the LLM emits, returned in `ChatEvent::ToolCallEnd`
//!
//! Doc 04's Agent layer translates between Doc 05 `Tool`s and these
//! Provider-level structs.

use serde::{Deserialize, Serialize};

use crate::schema::JsonSchema;

/// A tool *definition* we present to the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Stable name; used to match the model's tool_use response.
    ///
    /// **Must be non-empty.** An empty name makes the model's tool_use
    /// response unmatchable (nothing to key the dispatch on). The
    /// fields stay public for serde / ergonomic literals, but build via
    /// [`ToolSpec::new`] when the name comes from config/user input, and
    /// call [`validate`](Self::validate) at the provider boundary.
    pub name: String,
    /// What the tool does — the model uses this to decide *when* to call it.
    /// Per Doc 05 §3.3, this should explain "when to use", not just "what".
    pub description: String,
    /// JSON Schema for the input arguments object.
    pub input_schema: JsonSchema,
}

impl ToolSpec {
    /// Build a tool spec, rejecting an empty / whitespace-only `name`.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: JsonSchema,
    ) -> Result<Self, &'static str> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err("ToolSpec.name cannot be empty/whitespace");
        }
        Ok(Self {
            name,
            description: description.into(),
            input_schema,
        })
    }

    /// True iff `name` is non-empty (the matchability invariant). Call
    /// at the provider boundary before sending `request.tools` to the
    /// LLM.
    pub fn has_valid_name(&self) -> bool {
        !self.name.trim().is_empty()
    }
}

/// Constraint on tool selection.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call any tool.
    #[default]
    Auto,
    /// Model must not call any tool.
    None,
    /// Model must call any tool (it picks which).
    Required,
    /// Model must call this exact tool.
    Specific(String),
}

/// A single tool call emitted by the model.
///
/// **Crucial invariant**: `arguments` is always parsed JSON. Provider
/// adapters that receive string-encoded args (OpenAI) parse them
/// before constructing this struct. The Pipeline / Agent layer never
/// has to worry about double-decoding (Doc 01 §8).
#[derive(Clone, Debug, Serialize)]
pub struct ToolCall {
    /// Provider-side ID for correlating the call with its result message.
    /// Required by OpenAI and Anthropic; Gemini's adapter synthesizes one.
    pub id: String,
    pub name: String,
    /// Always a parsed object (`Value::Object`) — never a string.
    pub arguments: serde_json::Value,
}

impl<'de> Deserialize<'de> for ToolCall {
    /// Hand-rolled so the `arguments`-is-object invariant is enforced on
    /// the wire too. The derived `Deserialize` would happily accept a
    /// string / array / scalar `arguments`, bypassing the `new()`
    /// assert and letting a malformed `ToolCall` (that downstream code
    /// trusts to be an object) into the system (audit
    /// `tars-types-src-tools-3`).
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            id: String,
            name: String,
            arguments: serde_json::Value,
        }
        let raw = Raw::deserialize(deserializer)?;
        if !raw.arguments.is_object() {
            return Err(serde::de::Error::custom(
                "ToolCall.arguments must be a JSON object",
            ));
        }
        Ok(Self {
            id: raw.id,
            name: raw.name,
            arguments: raw.arguments,
        })
    }
}

impl ToolCall {
    /// Convenience constructor used by tests + adapters.
    ///
    /// **Runtime invariant**: `arguments` must be a JSON object.
    /// Anything else (string, array, bare scalar) violates the type's
    /// documented contract — downstream serialization to OpenAI's
    /// `tool_calls[].arguments` would emit malformed JSON strings,
    /// and downstream consumers (Agent layer) trust this invariant
    /// when indexing into args.
    ///
    /// We `assert!` (not `debug_assert!`) because the audit
    /// (`tars-types-src-tools-3`) flagged debug-only as silent in
    /// release. The adapter hot-path cost is one `Value::is_object()`
    /// call (a tag compare), well below noise.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        assert!(
            arguments.is_object(),
            "ToolCall.arguments must be a JSON object (got {:?})",
            arguments
        );
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    /// True iff `arguments` matches the documented invariant.
    /// Useful at trust boundaries (e.g. before sending to provider).
    pub fn args_are_object(&self) -> bool {
        self.arguments.is_object()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_choice_default_is_auto() {
        assert!(matches!(ToolChoice::default(), ToolChoice::Auto));
    }

    #[test]
    fn tool_call_serializes_arguments_as_object() {
        let call = ToolCall::new("call_1", "search", json!({"q": "rust"}));
        let v = serde_json::to_value(&call).unwrap();
        assert!(v["arguments"].is_object());
    }

    #[test]
    fn tool_call_deser_rejects_non_object_arguments() {
        // String / array / scalar arguments must not deserialize — they
        // would violate the documented object invariant.
        for bad in [
            r#"{"id":"c","name":"n","arguments":"not-an-object"}"#,
            r#"{"id":"c","name":"n","arguments":[1,2]}"#,
            r#"{"id":"c","name":"n","arguments":42}"#,
        ] {
            assert!(
                serde_json::from_str::<ToolCall>(bad).is_err(),
                "should reject non-object arguments: {bad}"
            );
        }
        // Object arguments still deserialize.
        let ok = r#"{"id":"c","name":"n","arguments":{"q":"rust"}}"#;
        assert!(serde_json::from_str::<ToolCall>(ok).is_ok());
    }
}
