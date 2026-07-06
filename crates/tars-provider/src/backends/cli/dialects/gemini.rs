//! [`GeminiCliDialect`] — the gemini-cli behavior expressed as a
//! [`CliDialect`] (Doc 32 §5 C3, M1). Reproduces the pre-migration
//! `gemini_cli` behavior byte-for-byte: the `gemini -p "<prompt>" -m <model>
//! -o json` argv (prompt as an **arg**, not stdin), the buffered single-JSON
//! output (with the decorative-prefix strip), and the `response`/`stats`
//! → `ChatEvent` mapping.
//!
//! The security win of M1: gemini — previously an **unconfined** black-box
//! agent — now spawns through the shared
//! [`SharedCliRunner`](super::super::subprocess::SharedCliRunner) +
//! `build_sandboxed_command` OS-jail primitive, so it gets the same
//! `tars-sandbox` write-jail as claude. Since Doc 32 §9's consolidation this
//! dialect declares only its argv + parse + [`OutputFraming`]; the buffered
//! spawn/drain is the shared runner's, not a bespoke gemini one.

use std::time::Duration;

use serde_json::Value;

use tars_types::{
    ChatEvent, ChatRequest, ContentBlock, Message, ProviderError, RequestContext, StopReason, Usage,
};

use super::super::argv::SubprocessInvocation;
use super::super::dialect::{CliDialect, CliInvocation, OutputFraming, OutputMode, PromptChannel};

/// Env vars that force `gemini` into direct API / Vertex mode and must be
/// stripped from the child to keep the subscription path active.
pub(crate) const STRIPPED_ENV_KEYS_UPPER: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_GENAI_USE_VERTEXAI",
];

/// Limit on the rendered prompt length passed via `-p`. ARG_MAX is typically
/// 1MB on Linux/macOS; we cap well below that so other args and env have
/// headroom. Above this we surface a clean `InvalidRequest` rather than
/// letting `execve` fail with E2BIG.
pub(crate) const MAX_PROMPT_BYTES: usize = 256 * 1024;

/// Gemini CLI dialect. Holds the per-invocation gemini configuration
/// (executable, timeout) that `gemini_cli`'s builder used to keep on the
/// provider; the shared `AgentCliBackend` stays free of any gemini specifics.
#[derive(Clone, Debug)]
pub struct GeminiCliDialect {
    executable: String,
    timeout: Duration,
}

impl GeminiCliDialect {
    pub fn new(executable: String, timeout: Duration) -> Self {
        Self {
            executable,
            timeout,
        }
    }
}

/// Construct the full `gemini` argv (without the executable) for an
/// invocation, used by [`GeminiCliDialect::argv`] (which the shared runner
/// calls) so the flag shape lives in exactly one place.
pub(crate) fn build_gemini_argv(inv: &SubprocessInvocation) -> Vec<String> {
    vec![
        "-p".into(),
        inv.prompt.clone(),
        "-m".into(),
        inv.model.clone(),
        "-o".into(),
        "json".into(),
    ]
}

impl CliDialect for GeminiCliDialect {
    fn argv(&self, inv: &CliInvocation) -> Vec<String> {
        build_gemini_argv(inv)
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        ctx: &RequestContext,
    ) -> Result<CliInvocation, ProviderError> {
        let model = req
            .model
            .explicit()
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "model must be explicit before reaching CLI provider".into(),
                )
            })?
            .to_string();

        let prompt = render_prompt_for_cli(req);
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(ProviderError::InvalidRequest(format!(
                "prompt size {} exceeds gemini CLI argv cap {} bytes",
                prompt.len(),
                MAX_PROMPT_BYTES
            )));
        }

        Ok(SubprocessInvocation::neutral(
            self.executable.clone(),
            model,
            prompt,
            self.timeout,
            STRIPPED_ENV_KEYS_UPPER.iter().map(|s| s.to_string()).collect(),
            // When the OS jail is on, gemini's process is confined to the
            // request's worktree (previously gemini ignored cwd → unconfined).
            ctx.cwd.clone(),
            ctx.sandbox.clone(),
        ))
    }

    fn prompt_channel(&self) -> PromptChannel {
        // `gemini -p "<prompt>"` — the prompt rides in the argv, not stdin.
        PromptChannel::Arg
    }

    fn output_mode(&self) -> OutputMode {
        OutputMode::JsonEvents
    }

    fn output_framing(&self) -> OutputFraming {
        // gemini prints decorative lines (e.g. "Ripgrep is not available…")
        // before its JSON, so the shared runner strips everything before the
        // first `{` and parses the single buffered object.
        OutputFraming::SingleObject { strip_prefix: true }
    }

    fn parse_line(&self, raw: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
        // The runner reconstructs gemini's `-o json` payload into a single
        // object. `response` is the answer text; `stats.models.<model>.tokens`
        // the token counts. The backend prepends `Started` and applies the
        // output-budget clamp, so the natural stop reason here is `EndTurn`.
        let text = extract_response_text(raw);
        let usage = extract_usage(raw, self.last_model(raw));
        Ok(vec![
            ChatEvent::Delta { text },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage,
            },
        ])
    }

    fn env(&self) -> &[&str] {
        // gemini strips auth env (via `stripped_env`) rather than adding any.
        &[]
    }
}

impl GeminiCliDialect {
    /// The model key for the `stats.models.<model>` usage lookup. The buffered
    /// payload carries the actual model name gemini billed under; parse it out
    /// of the single-key `stats.models` map (there's one entry per call). Falls
    /// back to empty (→ default usage) when absent.
    fn last_model<'a>(&self, payload: &'a Value) -> &'a str {
        payload
            .pointer("/stats/models")
            .and_then(|m| m.as_object())
            .and_then(|m| m.keys().next())
            .map(String::as_str)
            .unwrap_or("")
    }
}

/// Render a chat request as a single string prompt for the CLI. Embeds the
/// system prompt as a leading `[system]` block — the CLI has no
/// `--system-prompt` flag.
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
    // RFC 6901: `~` and `/` are reference-token metacharacters and must be
    // escaped (`~`→`~0`, `/`→`~1`) before interpolation, or a model name
    // containing them silently mis-resolves to None. Order matters: `~0` first
    // would double-escape the `/` replacement's intro.
    let escaped_model = model.replace('~', "~0").replace('/', "~1");
    let tokens = payload
        .pointer(&format!("/stats/models/{escaped_model}/tokens"))
        .and_then(|v| v.as_object());
    let tokens = match tokens {
        Some(t) => t,
        None => return Usage::default(),
    };
    Usage {
        input_tokens: tokens.get("prompt").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: tokens
            .get("candidates")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cached_input_tokens: tokens.get("cached").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_creation_tokens: 0,
        thinking_tokens: tokens.get("thoughts").and_then(|v| v.as_u64()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tars_types::{ModelHint, ModelTier};

    fn dialect() -> GeminiCliDialect {
        GeminiCliDialect::new("gemini".into(), Duration::from_secs(300))
    }

    #[test]
    fn argv_is_gemini_shape_with_prompt_as_arg() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), "say hi"),
                &RequestContext::test_default(),
            )
            .unwrap();
        let argv = d.argv(&inv);
        // `gemini -p "<prompt>" -m <model> -o json` — the byte-for-byte shape.
        assert_eq!(argv[0], "-p");
        assert!(argv[1].contains("say hi"));
        assert_eq!(argv[2], "-m");
        assert_eq!(argv[3], "gemini-2.5-flash");
        assert_eq!(argv[4], "-o");
        assert_eq!(argv[5], "json");
    }

    #[test]
    fn channel_is_arg_and_mode_is_json_events() {
        let d = dialect();
        assert_eq!(d.prompt_channel(), PromptChannel::Arg);
        assert_eq!(d.output_mode(), OutputMode::JsonEvents);
    }

    #[test]
    fn invocation_requires_explicit_model() {
        let d = dialect();
        let err = d
            .invocation(
                &ChatRequest::user(ModelHint::Tier(ModelTier::Default), "hi"),
                &RequestContext::test_default(),
            )
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn oversized_prompt_rejected_with_invalid_request() {
        let d = dialect();
        let big = "x".repeat(MAX_PROMPT_BYTES + 1);
        let err = d
            .invocation(
                &ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), big),
                &RequestContext::test_default(),
            )
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn invocation_strips_api_key_env_and_embeds_system() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user(ModelHint::Explicit("gemini-2.5-flash".into()), "x")
                    .with_system("be precise"),
                &RequestContext::test_default(),
            )
            .unwrap();
        assert!(inv.stripped_env.contains("GEMINI_API_KEY"));
        assert!(inv.stripped_env.contains("GOOGLE_API_KEY"));
        assert!(inv.stripped_env.contains("GOOGLE_APPLICATION_CREDENTIALS"));
        assert!(inv.prompt.starts_with("[system]\nbe precise"));
        assert!(inv.prompt.contains("[user]\nx"));
    }

    #[test]
    fn parse_line_maps_response_and_usage() {
        let d = dialect();
        let payload = json!({
            "session_id": "abc",
            "response": "Hello there!",
            "stats": {
                "models": {
                    "gemini-2.5-flash": {
                        "tokens": { "prompt": 50, "candidates": 4, "cached": 10, "thoughts": 7 }
                    }
                }
            }
        });
        let events = d.parse_line(&payload).unwrap();
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text == "Hello there!"));
        match &events[1] {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 50);
                assert_eq!(usage.output_tokens, 4);
                assert_eq!(usage.cached_input_tokens, 10);
                assert_eq!(usage.thinking_tokens, 7);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_missing_response_is_empty_delta() {
        let d = dialect();
        let events = d.parse_line(&json!({"session_id": "x", "stats": {}})).unwrap();
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text.is_empty()));
    }

    #[test]
    fn parse_line_missing_usage_is_default() {
        let d = dialect();
        let events = d.parse_line(&json!({"response": "ok"})).unwrap();
        match &events[1] {
            ChatEvent::Finished { usage, .. } => {
                assert_eq!(usage.input_tokens, 0);
                assert_eq!(usage.output_tokens, 0);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
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
