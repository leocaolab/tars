//! [`OpenCodeDialect`] — the `opencode` CLI behavior expressed as a
//! [`CliDialect`] (Doc 32 §5 C3, M2). Shells out to
//! `opencode run --format json --model <provider/model> <prompt>` (prompt as
//! the positional `message` **arg**), and maps opencode's `--format json`
//! event stream onto canonical [`ChatEvent`]s.
//!
//! `opencode` spawns through the shared
//! [`SharedCliRunner`](super::super::subprocess::SharedCliRunner) + OS-jail
//! primitive so — a black-box coding agent — it runs inside the same
//! `tars-sandbox` write-jail as the other delegates. This dialect declares only
//! argv + [`OutputFraming::JsonLinesArray`] + `parse_line`; no bespoke runner.
//!
//! ## opencode `--format json` event schema (grounded from
//! `packages/opencode/src/cli/cmd/run.ts` in the checkout at
//! `/Users/hucao/projects/opencode`)
//! Each stdout line is one JSON object: `{ type, timestamp, sessionID, ...data }`.
//! The `emit(type, data)` calls in `run.ts` produce exactly these `type`s in
//! JSON mode (all but `error` carry a `part`):
//! - `"text"`      → `{ part: TextPart }`; `part.text` is a COMPLETE assistant
//!   text block (only emitted once `part.time.end` is set) → [`ChatEvent::Delta`].
//! - `"reasoning"` → `{ part: ReasoningPart }`; `part.text` is the thinking text
//!   → [`ChatEvent::ThinkingDelta`].
//! - `"step_finish"` → `{ part: StepFinishPart }`; `part.tokens =
//!   { total?, input, output, reasoning, cache: { read, write } }` and `part.cost`.
//!   Carries the per-step token usage.
//! - `"step_start"` / `"tool_use"` → ignored (no canonical content).
//! - `"error"`     → `{ error: <session error> }` → a raw-carrying typed error.
//!
//! Note: JSON mode has **no explicit terminal event** — CONFIRMED from source.
//! `run.ts`'s `emit()` (the ONLY thing that writes to stdout) fires for exactly
//! `tool_use`/`step_start`/`step_finish`/`text`/`reasoning`/`error` and nothing
//! else; the read loop `break`s on the internal `session.status: idle` event
//! WITHOUT emitting it, and `message.updated` (which carries the assistant
//! message's turn-level `finish`) is gated on `args.format !== "json"` so it
//! never reaches stdout in JSON mode. So this dialect SYNTHESIZES the terminal
//! [`ChatEvent::Finished`] after the last line — that is the correct, source-
//! grounded behavior, not a fallback. Always [`StopReason::EndTurn`]: the only
//! finish reason opencode surfaces in JSON mode is a PER-STEP `reason` on each
//! `step-finish` part (`message-v2.ts`), never a turn-level one, so there is no
//! distinct turn stop reason to map.

use std::time::Duration;

use serde_json::Value;

use tars_types::{
    ChatEvent, ChatRequest, ContentBlock, Message, ProviderError, RequestContext, StopReason, Usage,
};

use super::super::argv::SubprocessInvocation;
use super::super::dialect::{CliDialect, CliInvocation, OutputFraming, OutputMode, PromptChannel};
use super::super::subprocess::truncate;

/// Limit on the rendered prompt length passed as the positional `message` arg
/// (it rides in the argv like gemini's `-p`). Cap well below ARG_MAX; above it
/// we surface a clean `InvalidRequest` rather than let `execve` fail E2BIG.
pub(crate) const MAX_PROMPT_BYTES: usize = 256 * 1024;

/// OpenCode CLI dialect. Holds the per-invocation config (executable, timeout);
/// the shared `AgentCliBackend` stays free of any opencode specifics.
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

/// Construct the full `opencode` argv (without the executable), used by
/// [`OpenCodeDialect::argv`] (which the shared runner calls) so the flag shape
/// lives in exactly one place. `--model` takes opencode's `provider/model` form
/// — whatever the user configured as `default_model`.
pub(crate) fn build_opencode_argv(inv: &SubprocessInvocation) -> Vec<String> {
    vec![
        "run".into(),
        "--format".into(),
        "json".into(),
        "--model".into(),
        inv.model.clone(),
        // Positional `message` — last so `--model`'s value isn't consumed by it.
        inv.prompt.clone(),
    ]
}

impl CliDialect for OpenCodeDialect {
    fn argv(&self, inv: &CliInvocation) -> Vec<String> {
        build_opencode_argv(inv)
    }

    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &RequestContext,
    ) -> Result<CliInvocation, ProviderError> {
        let model = model.to_string();

        let prompt = render_prompt_for_cli(req);
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(ProviderError::InvalidRequest(format!(
                "prompt size {} exceeds opencode CLI argv cap {} bytes",
                prompt.len(),
                MAX_PROMPT_BYTES
            )));
        }

        Ok(SubprocessInvocation::neutral(
            self.executable.clone(),
            model,
            prompt,
            ctx.call_budget(self.timeout),
            // opencode authenticates via its own `opencode auth login` /
            // provider env keys — nothing to strip.
            std::collections::HashSet::new(),
            // Jails opencode to the request's worktree and makes it its cwd
            // (opencode picks up the project from process.cwd()).
            ctx.cwd.clone(),
            ctx.sandbox.clone(),
        ))
    }

    fn prompt_channel(&self) -> PromptChannel {
        // `opencode run … "<prompt>"` — the prompt is the positional arg.
        PromptChannel::Arg
    }

    fn output_mode(&self) -> OutputMode {
        OutputMode::JsonEvents
    }

    fn output_framing(&self) -> OutputFraming {
        // opencode emits an NDJSON event stream; the shared runner buffers it
        // into a `Value::Array` of raw lines that `parse_line` maps per event.
        OutputFraming::JsonLinesArray
    }

    fn state_dirs(&self) -> Vec<std::path::PathBuf> {
        // opencode writes its own state OUTSIDE the worktree and blows up if the
        // jail denies it — the live failure was opening its log at
        // `~/.local/share/opencode/log`. It also reads/writes config + cache
        // under `~/.config/opencode` and `~/.cache/opencode`. Grant all three
        // (skipped if absent). `~` resolved via HOME.
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
        // The runner reconstructs opencode's NDJSON stream into a `Value::Array`
        // of raw line strings (same shape as codex). Map each line by its
        // `type`. The backend prepends `Started`, so we own the content Deltas +
        // the synthesized terminal `Finished`.
        let lines = raw.as_array().ok_or_else(|| {
            ProviderError::Parse(format!(
                "opencode runner payload must be a JSONL array, got: {}",
                truncate(&raw.to_string(), 200)
            ))
        })?;

        let mut out: Vec<ChatEvent> = Vec::new();
        // Usage is SUMMED across every `step_finish`. CONFIRMED from source
        // that per-step tokens are PER-STEP deltas, not a cumulative running
        // total: opencode's `processor.ts` "finish-step" handler assigns each
        // step-finish part `tokens = Session.getUsage(value.usage)` — a pure
        // stateless transform of THAT step's provider usage, with no
        // accumulator (contrast the sibling `cost += usage.cost`, which DOES
        // accumulate). So summing the per-step deltas yields the exact turn
        // total (single-step `opencode run` has exactly one `step_finish`).
        let mut usage = Usage::default();

        for line_val in lines {
            let line = line_val.as_str().unwrap_or_default();
            if line.trim().is_empty() {
                continue;
            }
            // Lenient parse: opencode may add fields/types we don't model. A
            // line we can't parse at all is skipped with a debug trace rather
            // than aborting the whole turn.
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
                        add_step_tokens(&mut usage, tokens);
                    }
                }
                "error" => {
                    // Carry the raw error out rather than substitute a sentinel
                    // (CLAUDE.md #1). `error` payload is the session error object.
                    let raw_err = v
                        .get("error")
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| line.to_string());
                    return Err(ProviderError::CliSubprocessDied {
                        exit_code: None,
                        stderr: format!("opencode error: {}", truncate(&raw_err, 300)),
                    });
                }
                // step_start, tool_use, and any future/unknown type: no
                // canonical content to surface.
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

/// Accumulate one `step_finish` part's `tokens` object into `usage`. opencode's
/// shape (`message-v2.ts` `StepFinishPart`):
/// `{ total?, input, output, reasoning, cache: { read, write } }`.
fn add_step_tokens(usage: &mut Usage, tokens: &Value) {
    let u64_at = |v: &Value, key: &str| v.get(key).and_then(|x| x.as_u64()).unwrap_or(0);
    usage.input_tokens += u64_at(tokens, "input");
    usage.output_tokens += u64_at(tokens, "output");
    usage.thinking_tokens += u64_at(tokens, "reasoning");
    if let Some(cache) = tokens.get("cache") {
        usage.cached_input_tokens += u64_at(cache, "read");
        usage.cache_creation_tokens += u64_at(cache, "write");
    }
}

/// Render a chat request as a single string prompt for the CLI. Embeds the
/// system prompt as a leading `[system]` block. Same shape as the other CLI
/// delegates.
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
                "anthropic/claude-sonnet-4-5", &RequestContext::test_default(),
            )
            .unwrap();
        let argv = d.argv(&inv);
        // `opencode run --format json --model <provider/model> "<prompt>"`.
        assert_eq!(argv[0], "run");
        assert_eq!(argv[1], "--format");
        assert_eq!(argv[2], "json");
        assert_eq!(argv[3], "--model");
        assert_eq!(argv[4], "anthropic/claude-sonnet-4-5");
        assert!(argv[5].contains("say hi"));
    }

    #[test]
    fn channel_is_arg_and_mode_is_json_events() {
        let d = dialect();
        assert_eq!(d.prompt_channel(), PromptChannel::Arg);
        assert_eq!(d.output_mode(), OutputMode::JsonEvents);
    }

    
    /// parse_line over a JSONL array (the runner's reconstructed shape).
    fn parse_line(lines: &[&str]) -> Result<Vec<ChatEvent>, ProviderError> {
        let arr = Value::Array(lines.iter().map(|l| Value::String(l.to_string())).collect());
        dialect().parse_line(&arr)
    }

    #[test]
    fn parse_line_maps_text_and_step_finish_usage() {
        // A realistic single-step opencode `--format json` stream.
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
                assert_eq!(usage.input_tokens, 50);
                assert_eq!(usage.output_tokens, 4);
                assert_eq!(usage.thinking_tokens, 7);
                assert_eq!(usage.cached_input_tokens, 10);
                assert_eq!(usage.cache_creation_tokens, 2);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
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
        // Terminal Finished synthesized even without a step_finish (default usage).
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
                assert_eq!(usage.input_tokens, 30);
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
        // Only the text Delta + synthesized Finished survive.
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
                // The raw error payload is carried out, not a sentinel.
                assert!(stderr.contains("missing api key"));
            }
            other => panic!("expected CliSubprocessDied, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_non_array_payload_is_typed_error() {
        let err = dialect().parse_line(&Value::String("not an array".into())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn invocation_embeds_system_as_prefix_block() {
        let inv = dialect()
            .invocation(
                &ChatRequest::user("x").with_system("be precise"),
                "anthropic/claude-sonnet-4-5", &RequestContext::test_default(),
            )
            .unwrap();
        assert!(inv.prompt.starts_with("[system]\nbe precise"));
        assert!(inv.prompt.contains("[user]\nx"));
    }
}
