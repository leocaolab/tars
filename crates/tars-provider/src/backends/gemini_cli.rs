//! Gemini CLI as an LLM Provider — subscription path.
//!
//! Subscription-authenticated path through the user-installed `gemini`
//! binary (google-gemini/gemini-cli). Same shape as [`claude_cli`]:
//! we shell out to the user's locally-authenticated CLI and never
//! touch the credentials.
//!
//! Binary interface (verified against gemini-cli ≥ 0.x):
//!
//! ```text
//! gemini -p "<prompt>" -m <model> -o json
//! ```
//!
//! Returns a JSON object like:
//! ```json
//! {
//!   "session_id": "…",
//!   "response": "<text>",
//!   "stats": {
//!     "models": {
//!       "<model-name>": {
//!         "tokens": { "prompt": N, "candidates": N, "cached": N, "thoughts": N }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! **Env strip**: any of the following force the CLI into direct API
//! mode (which bills the user's API key, not their subscription):
//! `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `GOOGLE_APPLICATION_CREDENTIALS`,
//! `GOOGLE_GENAI_USE_VERTEXAI`. Comparison is case-insensitive — same
//! Windows hazard the Claude CLI provider warns about.
//!
//! Streaming / cancellation are NOT supported in this iteration. The
//! `gemini` CLI does support `-o stream-json`, but a long-lived
//! process pool with mid-stream cancel is the next-iteration design
//! (analogous to Doc 01 §6.2.1 for Claude).

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

/// Env vars that force `gemini` into direct API / Vertex mode and must
/// be stripped from the child to keep the subscription path active.
const STRIPPED_ENV_KEYS_UPPER: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_GENAI_USE_VERTEXAI",
];

/// Limit on the rendered prompt length passed via `-p`. ARG_MAX is
/// typically 1MB on Linux/macOS; we cap well below that so other args
/// and env have headroom. Above this we surface a clean InvalidRequest
/// rather than letting `execve` fail with E2BIG.
const MAX_PROMPT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct GeminiCliProviderBuilder {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Option<Capabilities>,
}

impl GeminiCliProviderBuilder {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            executable: "gemini".to_string(),
            timeout: Duration::from_secs(300),
            capabilities: None,
        }
    }

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

    pub fn build(self) -> Arc<GeminiCliProvider> {
        self.build_with_runner(Arc::new(RealSubprocessRunner))
    }

    pub fn build_with_runner(
        self,
        runner: Arc<dyn SubprocessRunner>,
    ) -> Arc<GeminiCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        Arc::new(GeminiCliProvider {
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
        max_context_tokens: 1_048_576, // Gemini 2.5+ class
        max_output_tokens: 8_192,
        supports_tool_use: false, // CLI -p mode doesn't expose function calling
        supports_parallel_tool_calls: false,
        supports_structured_output: StructuredOutputMode::None,
        supports_vision: false,
        supports_thinking: false,
        supports_cancel: false, // spawn-per-call mode
        prompt_cache: PromptCacheKind::Delegated,
        streaming: false,
        modalities_in: text.clone(),
        modalities_out: text,
        pricing: tars_types::Pricing::default(),
    }
}

pub struct GeminiCliProvider {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Capabilities,
    runner: Arc<dyn SubprocessRunner>,
}

#[async_trait]
impl LlmProvider for GeminiCliProvider {
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

        let prompt = render_prompt_for_cli(&req);
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(ProviderError::InvalidRequest(format!(
                "prompt size {} exceeds gemini CLI argv cap {} bytes",
                prompt.len(),
                MAX_PROMPT_BYTES
            )));
        }

        let invocation = SubprocessInvocation {
            executable: self.executable.clone(),
            model: model.clone(),
            prompt,
            timeout: self.timeout,
            stripped_env: STRIPPED_ENV_KEYS_UPPER.iter().map(|s| s.to_string()).collect(),
        };

        let payload = self.runner.run(invocation).await?;
        let response_text = extract_response_text(&payload);
        let usage = extract_usage(&payload, &model);

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

#[derive(Clone, Debug)]
pub struct SubprocessInvocation {
    pub executable: String,
    pub model: String,
    pub prompt: String,
    pub timeout: Duration,
    pub stripped_env: HashSet<String>,
}

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
            .arg(&inv.prompt)
            .arg("-m")
            .arg(&inv.model)
            .arg("-o")
            .arg("json");

        cmd.env_clear();
        for (k, v) in std::env::vars() {
            if !inv.stripped_env.contains(&k.to_uppercase()) {
                cmd.env(k, v);
            }
        }

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(|e| match e.kind() {
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
                        "timed out after {}s (model={})",
                        inv.timeout.as_secs(),
                        inv.model
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
        // gemini-cli prepends decorative lines (e.g. "Ripgrep is not available…")
        // before the JSON. Strip everything before the first `{` so we get a
        // clean payload to parse.
        let json_start = stdout.find('{').unwrap_or(0);
        let json_text = &stdout[json_start..];

        let payload: Value = serde_json::from_str(json_text).map_err(|e| {
            ProviderError::Parse(format!(
                "gemini CLI non-JSON stdout: {e} (first 300: {})",
                truncate(&stdout, 300)
            ))
        })?;

        if !payload.is_object() {
            return Err(ProviderError::Parse(format!(
                "gemini CLI returned non-object JSON ({:?})",
                payload
            )));
        }

        Ok(payload)
    }
}

/// Render a chat request as a single string prompt for the CLI.
/// Embeds the system prompt as a leading `[system]` block — the CLI
/// has no `--system-prompt` flag.
fn render_prompt_for_cli(req: &ChatRequest) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(req.messages.len() + 1);
    if let Some(sys) = &req.system {
        parts.push(format!("[system]\n{sys}"));
    }
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

fn extract_response_text(payload: &Value) -> String {
    payload
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn extract_usage(payload: &Value, model: &str) -> Usage {
    let tokens = payload
        .pointer(&format!("/stats/models/{model}/tokens"))
        .and_then(|v| v.as_object());
    let tokens = match tokens {
        Some(t) => t,
        None => return Usage::default(),
    };
    Usage {
        input_tokens: tokens.get("prompt").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: tokens.get("candidates").and_then(|v| v.as_u64()).unwrap_or(0),
        cached_input_tokens: tokens.get("cached").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_creation_tokens: 0,
        thinking_tokens: tokens.get("thoughts").and_then(|v| v.as_u64()).unwrap_or(0),
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

/// Convenience builder.
pub fn gemini_cli(id: impl Into<ProviderId>) -> Arc<GeminiCliProvider> {
    GeminiCliProviderBuilder::new(id).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tars_types::ModelHint;

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

    fn make_provider(payload: Value) -> (Arc<GeminiCliProvider>, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            payload,
            recorded: std::sync::Mutex::new(None),
        });
        let p = GeminiCliProviderBuilder::new("gemini_cli_test")
            .build_with_runner(runner.clone());
        (p, runner)
    }

    #[tokio::test]
    async fn happy_path_returns_text_and_usage() {
        let payload = json!({
            "session_id": "abc",
            "response": "Hello there!",
            "stats": {
                "models": {
                    "gemini-2.5-flash": {
                        "tokens": {
                            "prompt": 50,
                            "candidates": 4,
                            "cached": 10,
                            "thoughts": 7,
                        }
                    }
                }
            }
        });
        let (provider, runner) = make_provider(payload);
        let resp = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), "say hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "Hello there!");
        assert_eq!(resp.usage.input_tokens, 50);
        assert_eq!(resp.usage.output_tokens, 4);
        assert_eq!(resp.usage.cached_input_tokens, 10);
        assert_eq!(resp.usage.thinking_tokens, 7);

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "gemini-2.5-flash");
        // Stripped env list MUST contain the API-mode-trigger keys.
        assert!(inv.stripped_env.contains("GEMINI_API_KEY"));
        assert!(inv.stripped_env.contains("GOOGLE_API_KEY"));
        assert!(inv.stripped_env.contains("GOOGLE_APPLICATION_CREDENTIALS"));
    }

    #[tokio::test]
    async fn missing_response_field_yields_empty_text() {
        let payload = json!({"session_id": "x", "stats": {}});
        let (provider, _) = make_provider(payload);
        let resp = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "");
    }

    #[tokio::test]
    async fn missing_usage_metadata_returns_default_usage() {
        let payload = json!({"response": "ok"});
        let (provider, _) = make_provider(payload);
        let resp = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.usage.input_tokens, 0);
        assert_eq!(resp.usage.output_tokens, 0);
    }

    #[tokio::test]
    async fn oversized_prompt_rejected_with_invalid_request() {
        let payload = json!({"response": "should never run"});
        let (provider, _) = make_provider(payload);
        let big = "x".repeat(MAX_PROMPT_BYTES + 1);
        let err = match provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), big),
                RequestContext::test_default(),
            )
            .await
        {
            Ok(_) => panic!("expected rejection"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn truncates_when_max_output_tokens_exceeded() {
        let big = "x".repeat(1000);
        let payload = json!({"response": big});
        let (provider, _) = make_provider(payload);
        let mut req = ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), "hi");
        req.max_output_tokens = Some(10);
        let resp = provider
            .complete(req, RequestContext::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text.len(), 40);
    }

    #[tokio::test]
    async fn system_message_is_embedded_as_prefix_block() {
        let payload = json!({"response": "ok"});
        let (provider, runner) = make_provider(payload);
        let _ = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), "x")
                    .with_system("be precise"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert!(inv.prompt.starts_with("[system]\nbe precise"));
        assert!(inv.prompt.contains("[user]\nx"));
    }

    #[test]
    fn env_strip_list_uppercase_and_includes_api_key_triggers() {
        for k in STRIPPED_ENV_KEYS_UPPER {
            assert_eq!(*k, k.to_uppercase());
        }
        assert!(STRIPPED_ENV_KEYS_UPPER.contains(&"GEMINI_API_KEY"));
        assert!(STRIPPED_ENV_KEYS_UPPER.contains(&"GOOGLE_API_KEY"));
    }
}
