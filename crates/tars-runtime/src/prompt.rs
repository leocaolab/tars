//! [`PromptBuilder`] — fluent assembly for the agent-facing
//! [`ChatRequest`].
//!
//! ## Why this exists
//!
//! By the end of M3 four agents (Orchestrator + Critic + Worker stub
//! + Worker tool-using) had hand-rolled the same six lines:
//!
//! ```ignore
//! let mut req = ChatRequest::user(ModelHint::Explicit(model.clone()), user_text);
//! req.system = Some(SYSTEM_PROMPT.to_string());
//! req.structured_output = Some(JsonSchema::strict(NAME, schema()));
//! req.temperature = Some(0.0);  // deterministic for cache + replay
//! req.tools = registry.to_tool_specs();  // worker only
//! ```
//!
//! That's the trigger-4 from `defer > delete > implement` for an
//! abstraction. [`PromptBuilder`] folds the recipe into one fluent
//! chain so a future request-shape addition (say, a new
//! `cache_directives` setting that should default the same way for
//! every agent) edits one place rather than four.
//!
//! ## What this is **not**
//!
//! Doc 04 §6's full design has PromptBuilder compose system prompts
//! from typed *blocks* (persona + role + tool-doc + format-rules),
//! so a tenant could rebrand the persona without touching the rest.
//! No agent today has multi-source prompts — building that machinery
//! before a consumer needs it is the exact speculative-abstraction
//! trap we avoid. The block-composition variant slots in once a
//! second persona ships (probably alongside multi-tenant work in M6).

use serde_json::Value;

use tars_types::{ChatRequest, JsonSchema, ModelHint, ToolSpec};

/// Fluent builder for the agent-facing [`ChatRequest`]. See module
/// docs for the consumer + scope rationale.
///
/// Built at the construction site of each agent's `build_*_request`
/// method; the resulting `ChatRequest` flows through `Agent::execute`
/// unchanged.
#[derive(Clone, Debug)]
pub struct PromptBuilder {
    model: String,
    user_text: String,
    system: Option<String>,
    structured_output: Option<JsonSchema>,
    temperature: Option<f32>,
    tools: Vec<ToolSpec>,
}

impl PromptBuilder {
    /// Start a new request for `model` with `user_text` as the single
    /// user-turn message. `model` becomes a `ModelHint::Explicit`
    /// (every TARS agent today picks its model concretely).
    pub fn new(model: impl Into<String>, user_text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            user_text: user_text.into(),
            system: None,
            structured_output: None,
            temperature: None,
            tools: Vec::new(),
        }
    }

    /// Set the system prompt.
    pub fn system(mut self, s: impl Into<String>) -> Self {
        self.system = Some(s.into());
        self
    }

    /// Force the model to emit JSON matching `schema`. Always uses
    /// strict mode (the strict-mode requirements — every property
    /// required, `additionalProperties: false` — are baked into the
    /// caller's `schema` argument; this method just wraps it in a
    /// strict [`JsonSchema`]).
    pub fn structured_output(mut self, name: impl Into<String>, schema: Value) -> Self {
        self.structured_output = Some(JsonSchema::strict(name, schema));
        self
    }

    /// Pin temperature to 0.0 for deterministic output. Every M3
    /// default agent uses this — same reasoning across the board:
    /// (a) cache layer needs determinism (cache key includes the
    /// request shape, equivalent prompts must yield equivalent
    /// requests) and (b) replay debugging is easier when the same
    /// goal yields the same response.
    pub fn deterministic(self) -> Self {
        self.temperature(0.0)
    }

    /// Override temperature explicitly. Mutually exclusive with
    /// [`Self::deterministic`] but the last call wins (no enforcement
    /// — they shouldn't be combined in practice).
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Advertise tools to the model. Empty `specs` is a no-op (same
    /// as the default), so callers can pass `registry.to_tool_specs()`
    /// unconditionally and rely on the registry being empty when
    /// they don't want tools.
    pub fn tools(mut self, specs: Vec<ToolSpec>) -> Self {
        self.tools = specs;
        self
    }

    /// Construct the final [`ChatRequest`].
    pub fn build(self) -> ChatRequest {
        let mut req = ChatRequest::user(ModelHint::Explicit(self.model), self.user_text);
        req.system = self.system;
        req.structured_output = self.structured_output;
        req.temperature = self.temperature;
        req.tools = self.tools;
        req
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_build_sets_model_and_user_text_only() {
        let req = PromptBuilder::new("gpt-4o", "hello").build();
        match &req.model {
            ModelHint::Explicit(m) => assert_eq!(m, "gpt-4o"),
            other => panic!("expected Explicit model, got {other:?}"),
        }
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content()[0].as_text(), Some("hello"));
        assert!(req.system.is_none());
        assert!(req.structured_output.is_none());
        assert!(req.temperature.is_none());
        assert!(req.tools.is_empty());
    }

    #[test]
    fn fluent_chain_threads_every_field() {
        let req = PromptBuilder::new("gpt-4o", "hello")
            .system("you are a helper")
            .structured_output("Reply", json!({"type": "object"}))
            .deterministic()
            .build();
        assert_eq!(req.system.as_deref(), Some("you are a helper"));
        assert_eq!(req.temperature, Some(0.0));
        let schema = req.structured_output.as_ref().unwrap();
        assert!(schema.strict);
        assert_eq!(schema.name.as_deref(), Some("Reply"));
    }

    #[test]
    fn deterministic_pins_temperature_to_zero() {
        let req = PromptBuilder::new("m", "u").deterministic().build();
        assert_eq!(req.temperature, Some(0.0));
    }

    #[test]
    fn temperature_override_supersedes_deterministic_when_called_last() {
        let req = PromptBuilder::new("m", "u")
            .deterministic()
            .temperature(0.7)
            .build();
        assert_eq!(req.temperature, Some(0.7));
    }

    #[test]
    fn empty_tools_vec_is_a_no_op() {
        let req = PromptBuilder::new("m", "u").tools(Vec::new()).build();
        assert!(req.tools.is_empty());
    }

    #[test]
    fn tools_get_threaded_into_request() {
        let spec = ToolSpec {
            name: "fs.read_file".into(),
            description: "read a file".into(),
            input_schema: JsonSchema::strict("Args", json!({"type": "object"})),
        };
        let req = PromptBuilder::new("m", "u").tools(vec![spec]).build();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "fs.read_file");
    }
}
