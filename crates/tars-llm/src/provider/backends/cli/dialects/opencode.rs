//! Talks to `opencode run --format json --model <provider/model> <prompt>`, the prompt as the last
//! argument. Per opencode's `packages/opencode/src/cli/cmd/run.ts`, each stdout line is one
//! object `{ type, timestamp, sessionID, ... }`:
//!
//! - `text`: `part.text` is a whole text block, sent once it ends; it becomes a
//!   [`ChatEvent::Delta`].
//! - `reasoning`: `part.text` is thinking; it becomes a [`ChatEvent::ThinkingDelta`].
//! - `step_finish`: `part.tokens` is `{ total?, input, output, reasoning, cache: { read, write } }`
//!   for that step alone.
//! - `step_start`, `tool_use`: carry nothing a reply needs.
//! - `error`: `error` is the session error; it fails the call.
//!
//! JSON mode prints no end-of-turn event and no turn-level stop reason, only one per step, so
//! the dialect adds `Finished` with `EndTurn` after the last line.

use std::time::Duration;

use serde_json::Value;

use crate::ProviderError;
use crate::provider::error::ParseProblem;
use tars_types::{
    Caller, ChatEvent, ChatRequest, ContentBlock, JsonKind, Message, StopReason, Usage,
};

use super::super::cli_subprocess_died;
use super::super::dialect::{CliDialect, OutputFraming, OutputMode, PromptChannel};
use super::super::invocation::SubprocessInvocation;
use super::super::prompt::check_prompt_arg;
use super::super::subprocess::truncate;

/// The `opencode` settings every run uses.
#[derive(Clone, Debug)]
pub struct OpenCodeDialect {
    executable: String,
    timeout: Duration,
}

impl OpenCodeDialect {
    pub fn new(executable: String, timeout: Duration) -> Self {
        Self {
            executable,
            timeout,
        }
    }
}

/// Returns `opencode`'s argv, without the executable. The model is in opencode's
/// `provider/model` form.
pub(crate) fn build_opencode_argv(inv: &SubprocessInvocation) -> Vec<String> {
    vec![
        "run".into(),
        "--format".into(),
        "json".into(),
        "--model".into(),
        inv.model.clone(),
        // Last, so it is not read as `--model`'s value.
        inv.prompt.clone(),
    ]
}

impl CliDialect for OpenCodeDialect {
    fn argv(&self, inv: &SubprocessInvocation) -> Vec<String> {
        build_opencode_argv(inv)
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &Caller,
    ) -> Result<SubprocessInvocation, ProviderError> {
        let model = model.to_string();

        let prompt = render_prompt_for_cli(req);
        check_prompt_arg(&prompt, "opencode")?;

        Ok(SubprocessInvocation::neutral(
            self.executable.clone(),
            model,
            prompt,
            ctx.call_budget(self.timeout),
            // opencode logs in with `opencode auth login` or the providers' own variables.
            std::collections::HashSet::new(),
            // opencode finds the project from its cwd.
            ctx.cwd.clone(),
            ctx.sandbox.clone(),
        ))
    }

    fn prompt_channel(&self) -> PromptChannel {
        PromptChannel::Arg
    }

    fn output_mode(&self) -> OutputMode {
        OutputMode::JsonEvents
    }

    fn output_framing(&self) -> OutputFraming {
        OutputFraming::JsonLinesArray
    }

    fn state_dirs(&self) -> Vec<std::path::PathBuf> {
        // opencode fails when it cannot open its log under `~/.local/share/opencode`; it also
        // writes config and cache under `~/.config` and `~/.cache`.
        let Some(home) = tars_types::env::home_dir() else {
            return Vec::new();
        };
        vec![
            home.join(".local/share/opencode"),
            home.join(".config/opencode"),
            home.join(".cache/opencode"),
        ]
    }

    fn parse_line(&self, raw: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
        let lines = raw.as_array().ok_or_else(|| ProviderError::Parse {
            at: "opencode runner payload",
            line: None,
            problem: Box::new(ParseProblem::WrongType {
                expected: JsonKind::Array,
                got: JsonKind::of(raw),
            }),
            raw: Some(truncate(&raw.to_string(), 200)),
        })?;

        let mut out: Vec<ChatEvent> = Vec::new();
        // Each `step_finish` counts its own step only (`processor.ts` sets it from that step's
        // usage), so the sum is the turn's.
        let mut usage = Usage::default();

        for line_val in lines {
            let line = line_val.as_str().unwrap_or_default();
            if line.trim().is_empty() {
                continue;
            }
            // A line that is not JSON is skipped, not fatal.
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(
                        line = %truncate(line, 200),
                        error = %e,
                        "opencode: skipping unparseable line",
                    );
                    continue;
                }
            };
            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
            match ty {
                "text" => {
                    if let Some(text) = v.pointer("/part/text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            out.push(ChatEvent::Delta {
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "reasoning" => {
                    if let Some(text) = v.pointer("/part/text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            out.push(ChatEvent::ThinkingDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "step_finish" => {
                    if let Some(tokens) = v.pointer("/part/tokens") {
                        usage = usage.merge(step_usage(tokens));
                    }
                }
                "error" => {
                    let raw_err = v
                        .get("error")
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| line.to_string());
                    return Err(cli_subprocess_died(
                        None,
                        format!("opencode error: {}", truncate(&raw_err, 300)),
                    ));
                }
                _ => {}
            }
        }

        out.push(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage,
        });
        Ok(out)
    }
}

/// Returns one step's `tokens` as a [`Usage`]. opencode subtracts the cache reads and writes from
/// `input` (`adjustedInputTokens` in its `session.ts`), so they are added back: in a `Usage` they
/// are part of `input_tokens`. A count opencode leaves out is 0.
fn step_usage(tokens: &Value) -> Usage {
    let u64_at = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cache = tokens.get("cache").unwrap_or(&Value::Null);
    let cached_input_tokens = u64_at(cache, "read");
    let cache_creation_tokens = u64_at(cache, "write");
    Usage {
        input_tokens: u64_at(tokens, "input")
            .saturating_add(cached_input_tokens)
            .saturating_add(cache_creation_tokens),
        output_tokens: u64_at(tokens, "output"),
        cached_input_tokens,
        cache_creation_tokens,
        thinking_tokens: u64_at(tokens, "reasoning"),
    }
}

/// Returns the request as one prompt under `[role]` lines, the system prompt first as a
/// `[system]` block.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dialect() -> OpenCodeDialect {
        OpenCodeDialect::new("opencode".into(), Duration::from_secs(300))
    }

    #[test]
    fn argv_is_opencode_shape_with_prompt_as_positional() {
        let d = dialect();
        let inv = d
            .invocation(
                &ChatRequest::user("say hi"),
                "anthropic/claude-sonnet-4-5",
                &Caller::test_default(),
            )
            .unwrap();
        let argv = d.argv(&inv);
        assert_eq!(argv[0], "run");
        assert_eq!(argv[1], "--format");
        assert_eq!(argv[2], "json");
        assert_eq!(argv[3], "--model");
        assert_eq!(argv[4], "anthropic/claude-sonnet-4-5");
        assert!(argv[5].contains("say hi"));
    }

    fn parse_line(lines: &[&str]) -> Result<Vec<ChatEvent>, ProviderError> {
        let arr = Value::Array(lines.iter().map(|l| Value::String(l.to_string())).collect());
        dialect().parse_line(&arr)
    }

    #[test]
    fn parse_line_maps_text_and_step_finish_usage() {
        let events = parse_line(&[
            r#"{"type":"step_start","timestamp":1,"sessionID":"s","part":{"type":"step-start"}}"#,
            r#"{"type":"text","timestamp":2,"sessionID":"s","part":{"type":"text","text":"Hello there!","time":{"start":1,"end":2}}}"#,
            r#"{"type":"step_finish","timestamp":3,"sessionID":"s","part":{"type":"step-finish","reason":"stop","cost":0.01,"tokens":{"input":50,"output":4,"reasoning":7,"cache":{"read":10,"write":2}}}}"#,
        ])
        .unwrap();
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text == "Hello there!"));
        match &events[1] {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                // 50 uncached plus the 10 read and 2 written that opencode took out.
                assert_eq!(usage.input_tokens, 62);
                assert_eq!(usage.output_tokens, 4);
                assert_eq!(usage.thinking_tokens, 7);
                assert_eq!(usage.cached_input_tokens, 10);
                assert_eq!(usage.cache_creation_tokens, 2);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn step_usage_counts_cache_reads_and_writes_inside_input() {
        let u = step_usage(&serde_json::json!({"input": 5, "cache": {"read": 3, "write": 2}}));
        assert_eq!(u.input_tokens, 10);
        assert!(u.cached_input_tokens + u.cache_creation_tokens <= u.input_tokens);
        let u = step_usage(&serde_json::json!({"input": 5}));
        assert_eq!(u.input_tokens, 5);
        assert_eq!(u.cached_input_tokens, 0);
    }

    #[test]
    fn parse_line_maps_reasoning_to_thinking_delta() {
        let events = parse_line(&[
            r#"{"type":"reasoning","sessionID":"s","part":{"type":"reasoning","text":"thinking…","time":{"start":1,"end":2}}}"#,
            r#"{"type":"text","sessionID":"s","part":{"type":"text","text":"answer","time":{"start":2,"end":3}}}"#,
        ])
        .unwrap();
        assert!(matches!(&events[0], ChatEvent::ThinkingDelta { text } if text == "thinking…"));
        assert!(matches!(&events[1], ChatEvent::Delta { text } if text == "answer"));
        match &events[2] {
            ChatEvent::Finished { usage, .. } => {
                assert_eq!(usage.input_tokens, 0);
                assert_eq!(usage.output_tokens, 0);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_sums_usage_across_multiple_step_finishes() {
        let events = parse_line(&[
            r#"{"type":"step_finish","part":{"tokens":{"input":10,"output":2,"reasoning":0,"cache":{"read":0,"write":0}}}}"#,
            r#"{"type":"text","part":{"type":"text","text":"done","time":{"end":9}}}"#,
            r#"{"type":"step_finish","part":{"tokens":{"input":20,"output":5,"reasoning":1,"cache":{"read":3,"write":0}}}}"#,
        ])
        .unwrap();
        match events.last().unwrap() {
            ChatEvent::Finished { usage, .. } => {
                assert_eq!(usage.input_tokens, 33);
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(usage.thinking_tokens, 1);
                assert_eq!(usage.cached_input_tokens, 3);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_skips_tool_use_step_start_and_blank_lines() {
        let events = parse_line(&[
            "",
            "   ",
            r#"{"type":"tool_use","part":{"type":"tool","tool":"read","state":{"status":"completed"}}}"#,
            r#"{"type":"step_start","part":{"type":"step-start"}}"#,
            r#"{"type":"text","part":{"type":"text","text":"only text","time":{"end":1}}}"#,
        ])
        .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ChatEvent::Delta { text } if text == "only text"));
    }

    #[test]
    fn parse_line_error_event_surfaces_raw_carrying_error() {
        let err = parse_line(&[
            r#"{"type":"error","sessionID":"s","error":{"name":"ProviderAuthError","data":{"message":"missing api key"}}}"#,
        ])
        .unwrap_err();
        match err {
            ProviderError::CliSubprocessDied { stderr, .. } => {
                assert!(stderr.contains("opencode error"));
                assert!(stderr.contains("missing api key"));
            }
            other => panic!("expected CliSubprocessDied, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_non_array_payload_is_typed_error() {
        let err = dialect()
            .parse_line(&Value::String("not an array".into()))
            .unwrap_err();
        assert!(matches!(err, ProviderError::Parse { .. }));
    }

    #[test]
    fn invocation_embeds_system_as_prefix_block() {
        let inv = dialect()
            .invocation(
                &ChatRequest::user("x").with_system("be precise"),
                "anthropic/claude-sonnet-4-5",
                &Caller::test_default(),
            )
            .unwrap();
        assert!(inv.prompt.starts_with("[system]\nbe precise"));
        assert!(inv.prompt.contains("[user]\nx"));
    }
}
