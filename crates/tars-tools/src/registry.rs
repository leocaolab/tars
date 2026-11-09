//! Holds tools by name, lists them as [`ToolSpec`]s for a request, and turns a [`ToolCall`] into
//! the [`Message::Tool`] the next turn sees.
//!
//! A failed call is a message, not an error: an unknown tool or a tool that fails becomes an
//! `is_error` message the model can recover from, so the agent loop goes on.
//!
//! It runs every tool it holds and does not decide whether a call is allowed: registering a
//! tool is granting it. A caller that needs a per-call decision grants the tool as a cap behind
//! a `tars_types::GatedCap`, whose gate decides.

use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use thiserror::Error;

use tars_types::{ContentBlock, Message, ToolCall, ToolSpec};

use tars_utils::display_chain;

use crate::tool::{Tool, ToolEnv, ToolError};

#[derive(Debug, Error)]
#[error("tool with name `{name}` is already registered")]
pub struct DuplicateToolError {
    pub name: String,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool.
    ///
    /// # Errors
    ///
    /// [`DuplicateToolError`] when a tool of that name is already registered; the first
    /// one stays.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), DuplicateToolError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(DuplicateToolError { name });
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn register_owned<T: Tool>(&mut self, tool: T) -> Result<(), DuplicateToolError> {
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

    /// Returns the [`ToolSpec`]s for [`tars_types::ChatRequest::tools`], sorted by name. The map's
    /// iteration order changes from run to run, and an unsorted list would change the request's
    /// fingerprint, so a cached or recorded call would miss.
    pub fn to_tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema().clone(),
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Runs one tool call and returns the [`Message::Tool`] for the next turn. An unknown tool, a
    /// [`ToolError`] and a cancellation all become an `is_error` message, logged at `warn`.
    pub async fn dispatch(
        &self,
        call: &ToolCall,
        env: &ToolEnv,
        cancel: CancellationToken,
    ) -> Message {
        let outcome = self.execute(call, env, cancel).await;
        let (title, content, is_error) = match outcome {
            Ok(result) => (result.title, result.content, result.is_error),
            Err(e) => {
                let msg = format!("tool error ({}): {}", e.classification(), display_chain(&e));
                tracing::warn!(
                    tool = %call.name,
                    call_id = %call.id,
                    error = %display_chain(&e),
                    "tool dispatch failed",
                );
                (None, msg, true)
            }
        };
        // The title goes to the log only; the model sees the content.
        if let Some(title) = title {
            tracing::info!(
                tool = %call.name,
                call_id = %call.id,
                is_error,
                title = %title,
                "tool dispatched",
            );
        }
        Message::Tool {
            tool_call_id: call.id.clone(),
            content: vec![ContentBlock::text(content)],
            is_error,
        }
    }

    async fn execute(
        &self,
        call: &ToolCall,
        env: &ToolEnv,
        cancel: CancellationToken,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| ToolError::UnknownTool {
                name: call.name.clone(),
            })?;
        tool.execute(
            serde_json::Value::Object(call.arguments.clone()),
            env,
            cancel,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::OnceLock;

    use tars_types::JsonSchema;

    use crate::tool::ToolOutput;

    /// Returns the outcome it was built with; an `Err` fails as a foreign tool with that message.
    struct EchoTool {
        name: &'static str,
        outcome: Result<ToolOutput, &'static str>,
    }

    impl EchoTool {
        fn ok(name: &'static str, content: &'static str) -> Self {
            Self {
                name,
                outcome: Ok(ToolOutput::success(content)),
            }
        }
        fn fails(name: &'static str) -> Self {
            Self {
                name,
                outcome: Err("nope"),
            }
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
            S.get_or_init(|| {
                JsonSchema::strict("Args", tars_types::json_object! {"type": "object"})
            })
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _env: &ToolEnv,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            self.outcome.clone().map_err(|message| ToolError::Foreign {
                message: message.to_string(),
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
            Err(DuplicateToolError { name }) => assert_eq!(name, "a"),
            other => panic!("expected Duplicate, got {other:?}"),
        }
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

    #[test]
    fn to_tool_specs_is_deterministically_ordered() {
        // Unsorted names across two registries, so an incidental order cannot pass.
        let names_in = [
            "fs.read_file",
            "bash.run",
            "ast.find",
            "fs.glob",
            "cargo.dep_source",
        ];
        let build = || {
            let mut reg = ToolRegistry::new();
            for n in names_in {
                reg.register_owned(EchoTool::ok(n, "x")).unwrap();
            }
            reg.to_tool_specs()
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
        };
        let a = build();
        let b = build();
        let mut want = names_in.map(String::from).to_vec();
        want.sort();
        assert_eq!(a, want, "projection must be sorted by name");
        assert_eq!(a, b, "two builds must produce the identical order");
    }

    #[tokio::test]
    async fn dispatch_happy_path_yields_tool_message_with_content() {
        let mut reg = ToolRegistry::new();
        reg.register_owned(EchoTool::ok("a", "result text"))
            .unwrap();
        let call = ToolCall::new(
            "call_1",
            "a",
            json!({"q": "x"}).as_object().unwrap().clone(),
        );
        let msg = reg
            .dispatch(&call, &ToolEnv::default(), CancellationToken::new())
            .await;
        match msg {
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert!(!is_error);
                assert_eq!(content[0].as_text(), Some("result text"));
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    use crate::SandboxPolicy;

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Records whether it ran and the `ctx.sandbox.network` it saw.
    struct RanTool {
        name: &'static str,
        ran: Arc<AtomicBool>,
        sandbox_network_seen: Arc<Mutex<Option<bool>>>,
    }
    impl RanTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                ran: Arc::new(AtomicBool::new(false)),
                sandbox_network_seen: Arc::new(Mutex::new(None)),
            }
        }
    }
    #[async_trait]
    impl Tool for RanTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "records that it ran"
        }
        fn input_schema(&self) -> &JsonSchema {
            static S: OnceLock<JsonSchema> = OnceLock::new();
            S.get_or_init(|| {
                JsonSchema::strict("Args", tars_types::json_object! {"type": "object"})
            })
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            env: &ToolEnv,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            *self.sandbox_network_seen.lock().unwrap() = Some(env.sandbox.network());
            self.ran.store(true, Ordering::SeqCst);
            Ok(ToolOutput::success("ran"))
        }
    }

    #[tokio::test]
    async fn tool_sees_sandbox_policy() {
        let tool = RanTool::new("a");
        let seen = tool.sandbox_network_seen.clone();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(tool)).unwrap();
        let ctx = ToolEnv {
            sandbox: SandboxPolicy::DangerFullAccess { network: false },
            ..Default::default()
        };
        let _ = reg
            .dispatch(
                &ToolCall::new("c1", "a", json!({}).as_object().unwrap().clone()),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            *seen.lock().unwrap(),
            Some(false),
            "tool must observe ctx.sandbox"
        );
    }

    #[tokio::test]
    async fn dispatch_lookup_miss_yields_is_error_message() {
        let reg = ToolRegistry::new();
        let call = ToolCall::new("call_1", "ghost", json!({}).as_object().unwrap().clone());
        let msg = reg
            .dispatch(&call, &ToolEnv::default(), CancellationToken::new())
            .await;
        match msg {
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert!(is_error, "lookup miss must produce is_error=true");
                let text = content[0].as_text().unwrap();
                assert!(
                    text.contains("ghost"),
                    "error text should mention the missing tool name"
                );
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_execute_failure_yields_is_error_message() {
        let mut reg = ToolRegistry::new();
        reg.register_owned(EchoTool::fails("a")).unwrap();
        let call = ToolCall::new("call_1", "a", json!({}).as_object().unwrap().clone());
        let msg = reg
            .dispatch(&call, &ToolEnv::default(), CancellationToken::new())
            .await;
        match msg {
            Message::Tool {
                is_error, content, ..
            } => {
                assert!(is_error);
                let text = content[0].as_text().unwrap();
                assert!(text.contains("nope"));
                assert!(
                    text.contains("execute"),
                    "error text should include the classification"
                );
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }
}
