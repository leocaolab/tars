//! [`ToolRegistry`] — name-keyed lookup + dispatch helper.
//!
//! Two responsibilities:
//!
//! 1. **Lookup table**. Holds `Arc<dyn Tool>` by name; produces
//!    [`ToolSpec`] vectors the agent can drop straight into
//!    [`tars_types::ChatRequest::tools`] so the model sees what's
//!    available.
//! 2. **Dispatch helper**. Given an LLM-emitted [`ToolCall`], looks
//!    up the tool, runs it, and packages the result as the
//!    [`Message::Tool`] the next LLM turn needs to see. Lookup-miss
//!    and execute-failure both produce an `is_error=true` message
//!    rather than bubbling up — the agent loop wants to feed
//!    *something* back so the model can adapt; an
//!    abort-on-tool-not-found would lose recoverable state.

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;

use tars_types::{ContentBlock, Message, ToolCall, ToolSpec};

use crate::tool::{Tool, ToolContext, ToolError};

/// Errors that escape the registry (i.e., aren't quietly turned into
/// `is_error=true` messages by the dispatcher). Today only one
/// variant — duplicate registration — because everything else maps
/// cleanly to a tool-error result.
#[derive(Debug, Error)]
pub enum ToolRegistryError {
    #[error("tool with name `{0}` is already registered")]
    Duplicate(String),
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Errors on duplicate name — silent overwrite would
    /// be a footgun (registering two `fs.read_file` impls + losing
    /// the first one quietly is exactly the kind of bug that takes
    /// hours to spot).
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(ToolRegistryError::Duplicate(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Convenience: register a fresh `Arc<T>` from owned `T`.
    pub fn register_owned<T: Tool>(&mut self, tool: T) -> Result<(), ToolRegistryError> {
        self.register(Arc::new(tool))
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Project the registry into [`ToolSpec`]s ready for
    /// [`tars_types::ChatRequest::tools`]. Order is unspecified —
    /// callers that need a stable order should sort.
    pub fn to_tool_specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema().clone(),
            })
            .collect()
    }

    /// Dispatch one LLM-emitted tool call into the [`Message::Tool`]
    /// the next turn needs.
    ///
    /// Lookup miss + execute error + cancellation all produce an
    /// `is_error=true` message rather than `Result::Err` — the agent
    /// loop wants something to feed back to the model so it can
    /// adapt. The only way the loop should drop out is if the
    /// underlying *agent* fails (LLM call errored), not if a tool
    /// did.
    pub async fn dispatch(&self, call: &ToolCall, ctx: ToolContext) -> Message {
        let outcome = self.execute(call, ctx).await;
        let (content, is_error) = match outcome {
            Ok(result) => (result.content, result.is_error),
            Err(e) => {
                let msg = format!("tool error ({}): {e}", e.classification());
                tracing::warn!(
                    tool = %call.name,
                    call_id = %call.id,
                    error = %e,
                    "tool dispatch failed",
                );
                (msg, true)
            }
        };
        Message::Tool {
            tool_call_id: call.id.clone(),
            content: vec![ContentBlock::text(content)],
            is_error,
        }
    }

    /// Internal: lookup + execute. Pulls out the
    /// `Result<ToolResult, ToolError>` shape so [`Self::dispatch`] can
    /// uniformly format both halves.
    async fn execute(
        &self,
        call: &ToolCall,
        ctx: ToolContext,
    ) -> Result<crate::tool::ToolResult, ToolError> {
        let tool = self.tools.get(&call.name).ok_or_else(|| {
            ToolError::Execute(format!("no tool registered with name `{}`", call.name))
        })?;
        tool.execute(call.arguments.clone(), ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::OnceLock;

    use tars_types::JsonSchema;

    use crate::tool::ToolResult;

    /// Stub Tool that records the args it saw + returns whatever the
    /// constructor was given. `Arc<Mutex<...>>` for thread safety
    /// since the dispatcher might be called from any task.
    struct EchoTool {
        name: &'static str,
        outcome: Result<ToolResult, ToolError>,
    }

    impl EchoTool {
        fn ok(name: &'static str, content: &'static str) -> Self {
            Self { name, outcome: Ok(ToolResult::success(content)) }
        }
        fn fails(name: &'static str) -> Self {
            Self { name, outcome: Err(ToolError::Execute("nope".into())) }
        }
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "test echo tool"
        }
        fn input_schema(&self) -> &JsonSchema {
            static S: OnceLock<JsonSchema> = OnceLock::new();
            S.get_or_init(|| JsonSchema::strict("Args", json!({"type": "object"})))
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            self.outcome.as_ref().map(Clone::clone).map_err(|e| match e {
                ToolError::Execute(s) => ToolError::Execute(s.clone()),
                ToolError::InvalidArguments(s) => ToolError::InvalidArguments(s.clone()),
                ToolError::Cancelled => ToolError::Cancelled,
            })
        }
    }

    #[test]
    fn register_inserts_and_lookup_finds() {
        let mut reg = ToolRegistry::new();
        reg.register_owned(EchoTool::ok("a", "x")).unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.get("a").is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn duplicate_registration_errors_loudly() {
        let mut reg = ToolRegistry::new();
        reg.register_owned(EchoTool::ok("a", "x")).unwrap();
        match reg.register_owned(EchoTool::ok("a", "y")) {
            Err(ToolRegistryError::Duplicate(name)) => assert_eq!(name, "a"),
            other => panic!("expected Duplicate, got {other:?}"),
        }
        // Original is preserved.
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn to_tool_specs_includes_every_tool() {
        let mut reg = ToolRegistry::new();
        reg.register_owned(EchoTool::ok("a", "x")).unwrap();
        reg.register_owned(EchoTool::ok("b", "y")).unwrap();
        let specs = reg.to_tool_specs();
        assert_eq!(specs.len(), 2);
        let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn dispatch_happy_path_yields_tool_message_with_content() {
        let mut reg = ToolRegistry::new();
        reg.register_owned(EchoTool::ok("a", "result text")).unwrap();
        let call = ToolCall::new("call_1", "a", json!({"q": "x"}));
        let msg = reg.dispatch(&call, ToolContext::default()).await;
        match msg {
            Message::Tool { tool_call_id, content, is_error } => {
                assert_eq!(tool_call_id, "call_1");
                assert!(!is_error);
                assert_eq!(content[0].as_text(), Some("result text"));
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_lookup_miss_yields_is_error_message() {
        let reg = ToolRegistry::new();
        let call = ToolCall::new("call_1", "ghost", json!({}));
        let msg = reg.dispatch(&call, ToolContext::default()).await;
        match msg {
            Message::Tool { tool_call_id, content, is_error } => {
                assert_eq!(tool_call_id, "call_1");
                assert!(is_error, "lookup miss must produce is_error=true");
                let text = content[0].as_text().unwrap();
                assert!(text.contains("ghost"), "error text should mention the missing tool name");
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_execute_failure_yields_is_error_message() {
        let mut reg = ToolRegistry::new();
        reg.register_owned(EchoTool::fails("a")).unwrap();
        let call = ToolCall::new("call_1", "a", json!({}));
        let msg = reg.dispatch(&call, ToolContext::default()).await;
        match msg {
            Message::Tool { is_error, content, .. } => {
                assert!(is_error);
                let text = content[0].as_text().unwrap();
                assert!(text.contains("nope"));
                assert!(text.contains("execute"), "error text should include the classification");
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }
}
