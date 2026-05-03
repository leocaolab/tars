//! Claude Code CLI as an LLM Provider — subscription path.
//!
//! Mirrors the Python `ClaudeSubprocessClient` in
//! `arc/app/llm/claude_subprocess_client.py`:
//!
//! - Shells out to `claude -p - --model X --output-format json
//!   --disable-slash-commands` and feeds the prompt on stdin.
//! - **Strips** `ANTHROPIC_API_KEY` (case-insensitive) and 3rd-party
//!   routing env vars before exec'ing the child. If any leak through,
//!   `claude` switches to API-billing mode and silently bills the
//!   wrong account.
//! - Parses the single JSON output from the CLI; surfaces typed errors
//!   on non-zero exit / malformed JSON / `is_error: true` payload.
//! - **Not yet streaming** — Doc 01 §6.2 calls for a long-lived process
//!   pool with `--output-format stream-json`; that's the next iteration
//!   (Doc 01 §6.2.1). This first cut spawns per call.
//!
//! Testability: the actual `Command::output()` call is behind a
//! [`SubprocessRunner`] trait so tests substitute a fake without
//! needing the real `claude` binary installed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use tars_types::{
    Capabilities, ChatRequest, ChatEvent, ContentBlock, Message, Modality, PromptCacheKind,
    ProviderError, ProviderId, RequestContext, StopReason, StructuredOutputMode, Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// Env vars that must NEVER leak into the child `claude` process.
/// Case-insensitive — Windows preserves env var case, so `Anthropic_Api_Key`
/// would slip past a literal-equality check (the Python comment is exactly
/// about this hazard).
const STRIPPED_ENV_KEYS_UPPER: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

#[derive(Clone, Debug)]
pub struct ClaudeCliProviderBuilder {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Option<Capabilities>,
}

impl ClaudeCliProviderBuilder {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            executable: "claude".to_string(),
            timeout: Duration::from_secs(300),
            capabilities: None,
        }
    }

    /// Override the binary path / name. Defaults to `claude` (PATH lookup).
    pub fn executable(mut self, e: impl Into<String>) -> Self {
        self.executable = e.into();
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn capabilities(mut self, c: Capabilities) -> Self {
        self.capabilities = Some(c);
        self
    }

    /// Build with the default real-process runner.
    pub fn build(self) -> Arc<ClaudeCliProvider> {
        self.build_with_runner(Arc::new(RealSubprocessRunner))
    }

    /// Build with a substituted runner — for tests.
    pub fn build_with_runner(
        self,
        runner: Arc<dyn SubprocessRunner>,
    ) -> Arc<ClaudeCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        Arc::new(ClaudeCliProvider {
            id: self.id,
            executable: self.executable,
            timeout: self.timeout,
            capabilities: caps,
            runner,
        })
    }
}

fn default_capabilities() -> Capabilities {
    let mut text = HashSet::new();
    text.insert(Modality::Text);
    Capabilities {
        max_context_tokens: 200_000,
        // CLI doesn't expose --max-output-tokens; we post-truncate.
        max_output_tokens: 64_000,
        supports_tool_use: false, // CLI -p mode doesn't expose tool use
        supports_parallel_tool_calls: false,
        supports_structured_output: StructuredOutputMode::None,
        supports_vision: false,
        supports_thinking: false,
        // First iteration: spawn-per-call; cancel works only via Drop
        // before the call begins. Mid-call cancel needs the long-lived
        // mode (Doc 01 §6.2.1).
        supports_cancel: false,
        prompt_cache: PromptCacheKind::Delegated,
        streaming: false,
        modalities_in: text.clone(),
        modalities_out: text,
        // Subscription-billed; per-token pricing N/A here.
        pricing: tars_types::Pricing::default(),
    }
}

pub struct ClaudeCliProvider {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Capabilities,
    runner: Arc<dyn SubprocessRunner>,
}

#[async_trait]
impl LlmProvider for ClaudeCliProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let model = req
            .model
            .explicit()
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "model must be explicit before reaching CLI provider".into(),
                )
            })?
            .to_string();

        let prompt = serialize_messages_for_cli(&req);
        let system = req.system.clone();

        let invocation = SubprocessInvocation {
            executable: self.executable.clone(),
            model: model.clone(),
            system,
            prompt,
            timeout: self.timeout,
            stripped_env: STRIPPED_ENV_KEYS_UPPER.iter().map(|s| s.to_string()).collect(),
        };

        let payload = self.runner.run(invocation).await?;
        let response_text = extract_result_text(&payload);
        let usage = extract_usage(&payload);

        let max_chars = req.max_output_tokens.map(|t| (t as usize) * 4);
        let truncated = match max_chars {
            Some(cap) if response_text.len() > cap => response_text[..cap].to_string(),
            _ => response_text,
        };

        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started(model)),
            Ok(ChatEvent::Delta { text: truncated }),
            Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage,
            }),
        ];

        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Single CLI invocation — what [`SubprocessRunner`] needs to know.
#[derive(Clone, Debug)]
pub struct SubprocessInvocation {
    pub executable: String,
    pub model: String,
    pub system: Option<String>,
    pub prompt: String,
    pub timeout: Duration,
    /// Env vars to strip from the child (UPPER-CASE for case-insensitive match).
    pub stripped_env: HashSet<String>,
}

/// Abstraction for "run `claude` and get back its JSON payload".
/// Production impl spawns a real subprocess; tests substitute a fake.
#[async_trait]
pub trait SubprocessRunner: Send + Sync {
    async fn run(&self, invocation: SubprocessInvocation) -> Result<Value, ProviderError>;
}

pub struct RealSubprocessRunner;

#[async_trait]
impl SubprocessRunner for RealSubprocessRunner {
    async fn run(&self, inv: SubprocessInvocation) -> Result<Value, ProviderError> {
        let mut cmd = Command::new(&inv.executable);
        cmd.arg("-p")
            .arg("-")
            .arg("--model")
            .arg(&inv.model)
            .arg("--output-format")
            .arg("json")
            .arg("--disable-slash-commands");
        if let Some(sys) = &inv.system {
            cmd.arg("--system-prompt").arg(sys);
        }

        // Strip the dangerous env vars CASE-INSENSITIVELY. Pass through everything else.
        cmd.env_clear();
        for (k, v) in std::env::vars() {
            if !inv.stripped_env.contains(&k.to_uppercase()) {
                cmd.env(k, v);
            }
        }

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("`{}` not found in PATH", inv.executable),
            },
            std::io::ErrorKind::PermissionDenied => ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("`{}` not executable: {e}", inv.executable),
            },
            _ => ProviderError::CliSubprocessDied {
                exit_code: None,
                stderr: format!("spawn failed: {e}"),
            },
        })?;

        // Write the prompt on stdin and close it.
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(inv.prompt.as_bytes())
                .await
                .map_err(|e| ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!("stdin write failed: {e}"),
                })?;
            // dropping `stdin` here closes the pipe so the child sees EOF
            drop(stdin);
        }

        // Wait with timeout.
        let output = match tokio::time::timeout(inv.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!("wait failed: {e}"),
                });
            }
            Err(_) => {
                return Err(ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!(
                        "timed out after {}s (model={}, prompt_chars={})",
                        inv.timeout.as_secs(),
                        inv.model,
                        inv.prompt.len()
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let truncated = if stderr.len() > 500 { stderr[..500].to_string() } else { stderr };
            return Err(ProviderError::CliSubprocessDied {
                exit_code: output.status.code(),
                stderr: truncated,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let payload: Value = serde_json::from_str(&stdout).map_err(|e| {
            ProviderError::Parse(format!(
                "claude CLI non-JSON stdout: {e} (first 300: {})",
                truncate(&stdout, 300)
            ))
        })?;

        if !payload.is_object() {
            return Err(ProviderError::Parse(format!(
                "claude CLI returned non-object JSON ({:?})",
                payload
            )));
        }

        if payload.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false) {
            let detail = payload
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("<no detail>")
                .to_string();
            return Err(ProviderError::CliSubprocessDied {
                exit_code: Some(0), // CLI signaled error in payload, not via exit code
                stderr: format!("claude CLI returned error: {}", truncate(&detail, 300)),
            });
        }

        Ok(payload)
    }
}

/// Flatten our message history into the single text blob the CLI expects.
/// Mirrors the Python `chat_multi` serializer ([role]\n content per turn).
fn serialize_messages_for_cli(req: &ChatRequest) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        let (role, content) = match m {
            Message::User { content } => ("user", content),
            Message::Assistant { content, .. } => ("assistant", content),
            Message::Tool { content, .. } => ("tool", content),
            Message::System { content } => ("system", content),
        };
        let flat = flatten_blocks(content);
        parts.push(format!("[{role}]\n{flat}"));
    }
    parts.join("\n\n")
}

fn flatten_blocks(blocks: &[ContentBlock]) -> String {
    let mut out: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } => out.push(text.clone()),
            ContentBlock::Image { mime, .. } => {
                out.push(format!("[image:{mime}]"));
            }
        }
    }
    out.join("\n")
}

/// CLI puts the response in `.result`. Python uses `payload.get("result") or ""`
/// to coerce JSON-null to empty string — same behavior here.
fn extract_result_text(payload: &Value) -> String {
    payload
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn extract_usage(payload: &Value) -> Usage {
    let usage = match payload.get("usage").and_then(|u| u.as_object()) {
        Some(u) => u,
        None => return Usage::default(),
    };
    Usage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cached_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        thinking_tokens: 0,
    }
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed = crate::http_base::truncate_utf8(s, max);
    if trimmed.len() == s.len() {
        s.to_string()
    } else {
        format!("{trimmed}…")
    }
}

/// Convenience builder — the most common path.
pub fn claude_cli(id: impl Into<ProviderId>) -> Arc<ClaudeCliProvider> {
    ClaudeCliProviderBuilder::new(id).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tars_types::ModelHint;

    /// Test runner that returns a canned payload and records the invocation.
    struct FakeRunner {
        payload: Value,
        recorded: std::sync::Mutex<Option<SubprocessInvocation>>,
    }

    #[async_trait]
    impl SubprocessRunner for FakeRunner {
        async fn run(&self, invocation: SubprocessInvocation) -> Result<Value, ProviderError> {
            *self.recorded.lock().unwrap() = Some(invocation);
            Ok(self.payload.clone())
        }
    }

    fn make_provider(payload: Value) -> (Arc<ClaudeCliProvider>, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            payload,
            recorded: std::sync::Mutex::new(None),
        });
        let p = ClaudeCliProviderBuilder::new("claude_cli_test")
            .build_with_runner(runner.clone());
        (p, runner)
    }

    #[tokio::test]
    async fn happy_path_returns_text_and_usage() {
        let payload = json!({
            "result": "hello from claude",
            "is_error": false,
            "usage": {
                "input_tokens": 12,
                "output_tokens": 5,
                "cache_read_input_tokens": 3
            }
        });
        let (provider, runner) = make_provider(payload);
        let resp = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "hello from claude");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 5);
        assert_eq!(resp.usage.cached_input_tokens, 3);

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "opus");
        // env strip list is correctly populated case-insensitively
        assert!(inv.stripped_env.contains("ANTHROPIC_API_KEY"));
        assert!(inv.stripped_env.contains("CLAUDE_CODE_USE_BEDROCK"));
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
        struct ErrRunner;
        #[async_trait]
        impl SubprocessRunner for ErrRunner {
            async fn run(&self, _: SubprocessInvocation) -> Result<Value, ProviderError> {
                Err(ProviderError::CliSubprocessDied {
                    exit_code: Some(0),
                    stderr: "claude CLI returned error: rate limited".into(),
                })
            }
        }
        let provider = ClaudeCliProviderBuilder::new("c")
            .build_with_runner(Arc::new(ErrRunner));
        let err = match provider
            .clone()
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "x"),
                RequestContext::test_default(),
            )
            .await
        {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::CliSubprocessDied { .. }));
    }

    #[tokio::test]
    async fn null_result_becomes_empty_text() {
        let payload = json!({"result": null, "is_error": false});
        let (provider, _) = make_provider(payload);
        let resp = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "");
    }

    #[tokio::test]
    async fn truncates_when_max_output_tokens_exceeded() {
        let big = "x".repeat(1000);
        let payload = json!({"result": big, "is_error": false});
        let (provider, _) = make_provider(payload);
        let mut req = ChatRequest::user(ModelHint::Explicit("opus".into()), "hi");
        req.max_output_tokens = Some(10); // ~40 chars at 4 chars/token
        let resp = provider
            .complete(req, RequestContext::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text.len(), 40);
    }

    #[tokio::test]
    async fn system_prompt_propagates_into_invocation() {
        let payload = json!({"result": "ok", "is_error": false});
        let (provider, runner) = make_provider(payload);
        let _ = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "x")
                    .with_system("be brief"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.system.as_deref(), Some("be brief"));
    }

    #[test]
    fn message_serializer_preserves_role_tags() {
        let req = ChatRequest {
            model: ModelHint::Explicit("x".into()),
            system: None,
            messages: vec![
                Message::user_text("first user turn"),
                Message::assistant_text("first assistant"),
                Message::user_text("second user turn"),
            ],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: Default::default(),
        };
        let s = serialize_messages_for_cli(&req);
        assert!(s.contains("[user]\nfirst user turn"));
        assert!(s.contains("[assistant]\nfirst assistant"));
        assert!(s.contains("[user]\nsecond user turn"));
    }

    #[test]
    fn env_strip_list_is_uppercase_for_case_insensitive_match() {
        for k in STRIPPED_ENV_KEYS_UPPER {
            assert_eq!(*k, k.to_uppercase());
        }
        // The list MUST include ANTHROPIC_API_KEY — that's the entire
        // point of CLI-mode auth.
        assert!(STRIPPED_ENV_KEYS_UPPER.contains(&"ANTHROPIC_API_KEY"));
    }
}
