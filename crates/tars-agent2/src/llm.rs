//! [`LlmAgent`] — the LLM-driven decider. **This is the one production agent, and it decides by
//! calling an LLM through [`tars_pipeline::LlmService`] — never raw HTTP.**
//!
//! Construction and call are the whole point of the reuse:
//! - the caller builds an [`LlmService`] via [`LlmService::builder`] (or `default_chain`) —
//!   binding `provider + model` and the middleware onion — and hands it to [`LlmAgent::new`];
//! - each [`Agent::step`] builds a [`ChatRequest`] from the rendered [`View`], calls
//!   `service.call(req, ctx)` (tars-pipeline/src/service.rs:62), and drains the returned
//!   [`tars_provider::LlmEventStream`], correlating `ToolCallStart{index,name}` with
//!   `ToolCallEnd{index,parsed_args}` (tars-types/src/events.rs:30-49) to build [`Intent`]s.
//!
//! `parsed_args` is already valid JSON (the provider adapter guarantees it), so a tool call maps
//! straight to an `Intent { component, handler, args }`. No tool calls this turn ⇒
//! [`Step::ProposeHalt`] — the god-program then verifies the gap against the world.

use std::collections::HashMap;

use async_trait::async_trait;
use futures::StreamExt;

use tars_pipeline::LlmService;
use tars_types::{ChatEvent, ChatRequest, JsonSchema, RequestContext, ToolChoice, ToolSpec};

use crate::agent::{Agent, Intent, Step};
use crate::render::View;
use crate::world::CompId;

/// An LLM-driven agent. Decides via [`LlmService`]; maps each returned tool call to an
/// [`Intent`] against a bound `(component, handler)`.
pub struct LlmAgent {
    /// The pipeline callable — `provider + model + middleware onion`, bound at construction.
    service: LlmService,
    /// The per-call context (trace / cancel / cwd). Cloned per step.
    ctx: RequestContext,
    /// System prompt: who the agent is + the operate protocol.
    system: String,
    /// Tool definitions presented to the model (`request.tools`).
    tools: Vec<ToolSpec>,
    /// tool name → the `(component, handler)` an intent targets when that tool is called.
    bindings: HashMap<String, (CompId, String)>,
}

impl LlmAgent {
    /// Build an agent over an already-constructed [`LlmService`]. Bind tools with
    /// [`LlmAgent::bind_tool`] before running.
    pub fn new(service: LlmService, ctx: RequestContext, system: impl Into<String>) -> Self {
        Self {
            service,
            ctx,
            system: system.into(),
            tools: Vec::new(),
            bindings: HashMap::new(),
        }
    }

    /// Bind a tool the model may call. `schema` is the JSON-Schema for the tool's args object
    /// (e.g. `{"type":"object","properties":{"content":{"type":"string"}},"required":["content"]}`).
    /// When the model calls `name`, the agent emits an `Intent { component, handler, args }` with
    /// `args` = the tool call's parsed JSON.
    pub fn bind_tool(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        schema: serde_json::Value,
        component: impl Into<CompId>,
        handler: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let spec = ToolSpec::new(name.clone(), description, JsonSchema::loose(schema))
            .expect("bound tool name must be non-empty");
        self.tools.push(spec);
        self.bindings
            .insert(name, (component.into(), handler.into()));
        self
    }

    /// Build the request for this crank: system prompt + the rendered view as the user turn +
    /// the bound tools. The model is NOT on the request — it is bound in the `LlmService`.
    fn build_request(&self, view: &View) -> ChatRequest {
        let mut req = ChatRequest::user(view.to_prompt()).with_system(self.system.clone());
        req.tools = self.tools.clone();
        req.tool_choice = ToolChoice::Auto;
        req
    }

    /// Map one completed tool call to an [`Intent`]. An unbound tool name still surfaces as a
    /// loud [`Intent`] (the world will fail it, carrying the raw args) rather than being dropped.
    fn intent_for(&self, name: &str, parsed_args: &serde_json::Value) -> Intent {
        match self.bindings.get(name) {
            Some((component, handler)) => {
                Intent::new(component.clone(), handler.clone(), parsed_args.to_string())
            }
            None => Intent::new(
                name.to_string(),
                "unknown_tool".to_string(),
                parsed_args.to_string(),
            ),
        }
    }
}

#[async_trait]
impl Agent for LlmAgent {
    async fn step(&mut self, view: &View) -> Step {
        let req = self.build_request(view);

        // THE call — through the pipeline, not raw HTTP.
        let mut stream = match self.service.call(req, self.ctx.clone()).await {
            Ok(s) => s,
            Err(e) => {
                // A provider error is not a fixed point and not a lie: surface it and let the
                // god-program decide. No move this crank → propose halt (it will re-verify).
                tracing::warn!("LlmAgent: LlmService.call failed: {e}");
                return Step::ProposeHalt;
            }
        };

        // Correlate ToolCallStart{index,name} → ToolCallEnd{index,parsed_args} by index.
        let mut names: HashMap<usize, String> = HashMap::new();
        let mut intents: Vec<Intent> = Vec::new();

        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ChatEvent::ToolCallStart { index, name, .. }) => {
                    names.insert(index, name);
                }
                Ok(ChatEvent::ToolCallEnd {
                    index, parsed_args, ..
                }) => {
                    let name = names.get(&index).cloned().unwrap_or_default();
                    intents.push(self.intent_for(&name, &parsed_args));
                }
                Ok(_) => {}
                Err(e) => {
                    // Surface the stream error truthfully; keep any intents already parsed.
                    tracing::warn!("LlmAgent: stream error: {e}");
                }
            }
        }

        if intents.is_empty() {
            // No tool call → the agent has no productive move this crank. The god-program
            // verifies the gap against the world before honoring this.
            Step::ProposeHalt
        } else {
            Step::Emit(intents)
        }
    }
}
