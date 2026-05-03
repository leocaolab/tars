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
    pub name: String,
    /// What the tool does — the model uses this to decide *when* to call it.
    /// Per Doc 05 §3.3, this should explain "when to use", not just "what".
    pub description: String,
    /// JSON Schema for the input arguments object.
    pub input_schema: JsonSchema,
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-side ID for correlating the call with its result message.
    /// Required by OpenAI and Anthropic; Gemini's adapter synthesizes one.
    pub id: String,
    pub name: String,
    /// Always a parsed object (`Value::Object`) — never a string.
    pub arguments: serde_json::Value,
}

impl ToolCall {
    /// Convenience constructor used by tests + adapters.
    ///
    /// **Debug-builds-only invariant**: `arguments` must be a JSON
    /// object. Anything else (string, array, bare scalar) violates the
    /// type's documented contract — production calls into the model
    /// would emit malformed `tool_calls.arguments` strings. We
    /// `debug_assert!` rather than panic in release because adapter
    /// code that constructs many ToolCalls in hot paths shouldn't pay
    /// for this check on every release-build call. Audit findings
    /// `tars-types-src-tools-{4,5}`.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        debug_assert!(
            arguments.is_object(),
            "ToolCall.arguments must be a JSON object (got {:?})",
            arguments
        );
        Self { id: id.into(), name: name.into(), arguments }
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
}
