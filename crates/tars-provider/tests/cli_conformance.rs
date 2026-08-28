//! Cross-dialect CLI-delegate conformance suite.
//! What the suite proves is UNIFORM across every dialect:
//!   1. **Declarations.** Each dialect declares a `(PromptChannel, OutputMode,
//!      OutputFraming)` triple and the backend/runner honor it.
//!   2. **Env-strip.** The dialect's declared auth-env keys all land in
//!      `SubprocessInvocation.stripped_env`, are UPPER-CASE (the
//!      case-insensitive-match contract), and are the WHOLE strip set (so a
//!      passthrough dialect — antigravity/opencode — strips nothing).
//!   3. **Timeout / `ctx.call_budget`.** The caller's deadline WINS over the
//!      configured timeout (longer buys a longer run, shorter cuts it off, an
//!      expired deadline saturates to ZERO); with no deadline the configured
//!      timeout is the default. A subprocess outlives its future, so this leaf
//!      is the only place that can enforce it.
//!   4. **Success decode.** A well-formed runner payload flows through
//!      `AgentCliBackend::stream` as `Started(model)` → a `Delta` carrying the
//!      answer text → terminal `Finished(EndTurn)` — regardless of whether the
//!      dialect frames its output as a single JSON object, a JSONL array, or
//!      raw text.
//!   5. **Dead subprocess.** A runner that returns
//!      [`ProviderError::CliSubprocessDied`] surfaces that typed error through
//!      the backend unchanged (the backend's error path is dialect-agnostic).
//!   6. **Prompt-size cap.** An arg-channel dialect that caps its prompt (the
//!      prompt rides in argv → `E2BIG` risk) rejects an oversized prompt with
//!      `InvalidRequest`; a stdin dialect that has no such cap accepts a large
//!      prompt.
//!
//! What stays in each dialect's own `#[cfg(test)]` mod (NOT folded here,
//! because it is dialect-UNIQUE, not a cross-backend invariant):
//!   - the exact argv each dialect builds (flag names, ordering, `--sandbox`
//!     / `--add-dir` / `--skip-git-repo-check` quirks),
//!   - each dialect's own parse edge-cases + token-usage math (claude's cache
//!     fold, codex's `ThreadEvent` map + negative-token clamp,
//!     opencode's per-step usage summation),
//!   - antigravity's auth-env *passthrough* naming (`env()`).
//!
//! Adding a new CLI-delegate backend means: implement `CliScenarios` for it,
//! add one `cli_conformance_suite!(name, MyScenarios);` line, done.

#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};

use tars_provider::backends::cli::{
    AgentCliBackend, AntigravityDialect, AntigravityEffort, ClaudeCliDialect, ClaudeCliTools,
    CliDialect, CodexCliDialect, OpenCodeDialect, OutputFraming, OutputMode, PromptChannel,
    SandboxMode, SubprocessInvocation, SubprocessRunner,
};
use tars_provider::provider::LlmProvider;
use tars_types::{
    ChatEvent, ChatRequest, Pricing, ProviderError, ProviderProfile, RequestContext, StopReason,
};

/// The configured per-provider timeout every scenario builds its dialect with.
/// A distinctive value so the "no deadline ⇒ configured default" assertion is
/// unambiguous (not accidentally equal to a deadline-derived budget).
const CONFIG_TIMEOUT: Duration = Duration::from_secs(137);

/// The answer text every scenario's success payload carries — asserted verbatim
/// on the way out as the backend's single content `Delta`.
const ANSWER: &str = "conformance answer";

// ──────────────────────────────────────────────────────────────────────
// Fake runners.
// A dialect never spawns anything here; the runner hands back a canned payload
// (the exact shape the real `SharedCliRunner`/`RealSubprocessRunner` would
// reconstruct for that dialect's declared framing) or a typed failure.
// ──────────────────────────────────────────────────────────────────────

struct OkRunner {
    payload: Value,
}

#[async_trait]
impl SubprocessRunner for OkRunner {
    async fn run(&self, _inv: SubprocessInvocation) -> Result<Value, ProviderError> {
        Ok(self.payload.clone())
    }
}

struct DeadRunner;

#[async_trait]
impl SubprocessRunner for DeadRunner {
    async fn run(&self, _inv: SubprocessInvocation) -> Result<Value, ProviderError> {
        Err(ProviderError::CliSubprocessDied {
            exit_code: Some(0),
            stderr: "conformance: simulated dead subprocess".into(),
        })
    }
}

fn backend(
    dialect: Arc<dyn CliDialect>,
    runner: Arc<dyn SubprocessRunner>,
) -> Arc<AgentCliBackend> {
    Arc::new(AgentCliBackend::new(
        "cli_conf".into(),
        ProviderProfile::text_only_baseline(Pricing::default()),
        dialect,
        runner,
    ))
}

/// Encode NDJSON lines as the `Value::Array` of raw line strings the shared
/// runner produces for a `JsonLinesArray` dialect (codex / opencode).
fn jsonl(lines: &[&str]) -> Value {
    Value::Array(lines.iter().map(|l| Value::String((*l).into())).collect())
}

// ──────────────────────────────────────────────────────────────────────
// Per-dialect scenarios — each a unit struct used as a namespace.
// ──────────────────────────────────────────────────────────────────────

mod scenarios {
    use super::*;

    pub struct Claude;
    impl Claude {
        pub fn dialect() -> Arc<dyn CliDialect> {
            Arc::new(ClaudeCliDialect::new(
                "claude".into(),
                CONFIG_TIMEOUT,
                ClaudeCliTools::Disabled,
                false,
                None,
                true,
                Vec::new(),
            ))
        }
        pub fn model() -> &'static str {
            "opus"
        }
        pub fn declarations() -> (PromptChannel, OutputMode, OutputFraming) {
            (
                PromptChannel::Stdin,
                OutputMode::JsonEvents,
                OutputFraming::SingleObject {
                    strip_prefix: false,
                },
            )
        }
        pub fn stripped_env() -> &'static [&'static str] {
            &[
                "ANTHROPIC_API_KEY",
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "CLAUDE_CODE_USE_FOUNDRY",
            ]
        }
        pub fn prompt_cap() -> Option<usize> {
            None // prompt goes on stdin — no argv size limit
        }
        pub fn success_payload() -> Value {
            json!({
                "result": ANSWER,
                "is_error": false,
                "usage": { "input_tokens": 7, "output_tokens": 3 }
            })
        }
    }

    pub struct Codex;
    impl Codex {
        pub fn dialect() -> Arc<dyn CliDialect> {
            Arc::new(CodexCliDialect::new(
                "codex".into(),
                CONFIG_TIMEOUT,
                SandboxMode::ReadOnly,
                true,
            ))
        }
        pub fn model() -> &'static str {
            "gpt-5"
        }
        pub fn declarations() -> (PromptChannel, OutputMode, OutputFraming) {
            (
                PromptChannel::Stdin,
                OutputMode::JsonEvents,
                OutputFraming::JsonLinesArray,
            )
        }
        pub fn stripped_env() -> &'static [&'static str] {
            &["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_AGENT_IDENTITY"]
        }
        pub fn prompt_cap() -> Option<usize> {
            None // prompt goes on stdin
        }
        pub fn success_payload() -> Value {
            jsonl(&[
                r#"{"type":"thread.started","thread_id":"t1"}"#,
                r#"{"type":"turn.started"}"#,
                &format!(
                    r#"{{"type":"item.completed","item":{{"id":"i1","type":"agent_message","text":"{ANSWER}"}}}}"#
                ),
                r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2,"reasoning_output_tokens":0}}"#,
            ])
        }
    }

    pub struct OpenCode;
    impl OpenCode {
        pub fn dialect() -> Arc<dyn CliDialect> {
            Arc::new(OpenCodeDialect::new("opencode".into(), CONFIG_TIMEOUT))
        }
        pub fn model() -> &'static str {
            "anthropic/claude-sonnet-4-5"
        }
        pub fn declarations() -> (PromptChannel, OutputMode, OutputFraming) {
            (
                PromptChannel::Arg,
                OutputMode::JsonEvents,
                OutputFraming::JsonLinesArray,
            )
        }
        pub fn stripped_env() -> &'static [&'static str] {
            &[] // opencode authenticates via its own login — nothing to strip
        }
        pub fn prompt_cap() -> Option<usize> {
            Some(256 * 1024) // prompt is the positional `message` argv token
        }
        pub fn success_payload() -> Value {
            jsonl(&[
                r#"{"type":"step_start","part":{"type":"step-start"}}"#,
                &format!(
                    r#"{{"type":"text","part":{{"type":"text","text":"{ANSWER}","time":{{"start":1,"end":2}}}}}}"#
                ),
                r#"{"type":"step_finish","part":{"type":"step-finish","tokens":{"input":50,"output":4,"reasoning":0,"cache":{"read":0,"write":0}}}}"#,
            ])
        }
    }

    pub struct Antigravity;
    impl Antigravity {
        pub fn dialect() -> Arc<dyn CliDialect> {
            Arc::new(AntigravityDialect::new(
                "agy".into(),
                CONFIG_TIMEOUT,
                AntigravityEffort::High,
            ))
        }
        pub fn model() -> &'static str {
            "gemini-2.5-pro"
        }
        pub fn declarations() -> (PromptChannel, OutputMode, OutputFraming) {
            (PromptChannel::Arg, OutputMode::Text, OutputFraming::RawText)
        }
        pub fn stripped_env() -> &'static [&'static str] {
            &[] // agy's auth env passes THROUGH (see the dialect's `env()` test)
        }
        pub fn prompt_cap() -> Option<usize> {
            Some(256 * 1024) // prompt rides in argv (`-p "<prompt>"`)
        }
        pub fn success_payload() -> Value {
            // OutputMode::Text: the runner hands raw stdout to the backend as a
            // JSON string; agy prints the answer + a trailing newline.
            Value::String(format!("{ANSWER}\n"))
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// The shared test body — instantiated once per dialect.
// ──────────────────────────────────────────────────────────────────────

macro_rules! cli_conformance_suite {
    ($name:ident, $scenarios:ty) => {
        mod $name {
            use super::*;

            // ── 1. Declarations: (channel, mode, framing) ──────────────
            #[test]
            fn declares_channel_mode_framing() {
                let d = <$scenarios>::dialect();
                let (channel, mode, framing) = <$scenarios>::declarations();
                assert_eq!(d.prompt_channel(), channel, "prompt channel");
                assert_eq!(d.output_mode(), mode, "output mode");
                assert_eq!(d.output_framing(), framing, "output framing");
            }

            // ── 2. Env-strip: declared keys, UPPER-CASE, whole set ──────
            #[test]
            fn strips_exactly_the_declared_auth_env() {
                let d = <$scenarios>::dialect();
                let inv = d
                    .invocation(
                        &ChatRequest::user("hi"),
                        <$scenarios>::model(),
                        &RequestContext::test_default(),
                    )
                    .unwrap();
                let expected = <$scenarios>::stripped_env();
                for key in expected {
                    assert!(
                        inv.stripped_env.contains(*key),
                        "stripped_env must contain {key}",
                    );
                    assert_eq!(
                        *key,
                        key.to_uppercase(),
                        "strip keys must be UPPER-CASE (case-insensitive match contract)",
                    );
                }
                // The declared set is the WHOLE strip set — a passthrough
                // dialect (empty declaration) must strip nothing.
                assert_eq!(
                    inv.stripped_env.len(),
                    expected.len(),
                    "stripped_env must equal exactly the declared key set, got {:?}",
                    inv.stripped_env,
                );
            }

            // ── 3. Timeout: ctx.call_budget wins over configured timeout ─
            #[test]
            fn caller_deadline_wins_over_configured_timeout() {
                let d = <$scenarios>::dialect();
                let model = <$scenarios>::model();

                // No deadline ⇒ the configured timeout is the default.
                let inv = d
                    .invocation(&ChatRequest::user("x"), model, &RequestContext::test_default())
                    .unwrap();
                assert_eq!(
                    inv.timeout, CONFIG_TIMEOUT,
                    "no deadline ⇒ configured timeout is the default",
                );

                // A longer deadline buys a longer run than the config.
                let mut ctx = RequestContext::test_default();
                ctx.deadline = Some(Instant::now() + Duration::from_secs(600));
                let inv = d.invocation(&ChatRequest::user("x"), model, &ctx).unwrap();
                assert!(
                    inv.timeout > Duration::from_secs(500) && inv.timeout <= Duration::from_secs(600),
                    "longer deadline must set the budget (not clamp to config), got {:?}",
                    inv.timeout,
                );

                // A shorter deadline cuts the call off early.
                let mut ctx = RequestContext::test_default();
                ctx.deadline = Some(Instant::now() + Duration::from_secs(5));
                let inv = d.invocation(&ChatRequest::user("x"), model, &ctx).unwrap();
                assert!(inv.timeout <= Duration::from_secs(5), "got {:?}", inv.timeout);

                // An expired deadline saturates to ZERO — don't spawn a full run
                // there is no time for.
                let mut ctx = RequestContext::test_default();
                ctx.deadline = Some(Instant::now() - Duration::from_secs(1));
                let inv = d.invocation(&ChatRequest::user("x"), model, &ctx).unwrap();
                assert_eq!(inv.timeout, Duration::ZERO, "expired deadline ⇒ ZERO budget");
            }

            // ── 4. Success decode: Started → Delta(text) → Finished ─────
            #[tokio::test]
            async fn success_payload_yields_started_delta_finished() {
                let backend = backend(
                    <$scenarios>::dialect(),
                    Arc::new(OkRunner { payload: <$scenarios>::success_payload() }),
                );
                let model = <$scenarios>::model();

                let events: Vec<ChatEvent> = Arc::clone(&backend)
                    .stream(ChatRequest::user("hi"), model, RequestContext::test_default())
                    .await
                    .expect("stream should open")
                    .map(|e| e.expect("no error event"))
                    .collect()
                    .await;


                assert!(
                    matches!(&events[0], ChatEvent::Started { actual_model, .. } if actual_model == model),
                    "first event must be Started({model}), got {:?}",
                    events[0],
                );

                assert!(
                    events.iter().any(|e| matches!(e, ChatEvent::Delta { text } if text == ANSWER)),
                    "a Delta must carry the answer text, got {events:?}",
                );

                assert!(
                    matches!(
                        events.last(),
                        Some(ChatEvent::Finished { stop_reason, .. }) if *stop_reason == StopReason::EndTurn
                    ),
                    "last event must be Finished(EndTurn), got {:?}",
                    events.last(),
                );
            }

            // ── 5. Dead subprocess → CliSubprocessDied passes through ───
            #[tokio::test]
            async fn dead_subprocess_error_propagates() {
                let backend = backend(<$scenarios>::dialect(), Arc::new(DeadRunner));
                let err = backend
                    .complete(
                        ChatRequest::user("hi"),
                        <$scenarios>::model(),
                        RequestContext::test_default(),
                    )
                    .await
                    .expect_err("a dead subprocess must surface an error");
                assert!(
                    matches!(err, ProviderError::CliSubprocessDied { .. }),
                    "dead subprocess must map to CliSubprocessDied, got {err:?}",
                );
            }

            // ── 6. Prompt-size cap (arg-channel dialects) ───────────────
            #[test]
            fn prompt_size_cap_is_enforced() {
                let d = <$scenarios>::dialect();
                let model = <$scenarios>::model();
                match <$scenarios>::prompt_cap() {
                    Some(cap) => {
                        // An arg-channel dialect caps the prompt (it rides in
                        // argv → E2BIG). Just over the cap ⇒ a clean InvalidRequest.
                        let big = "x".repeat(cap + 1);
                        let err = d
                            .invocation(&ChatRequest::user(big), model, &RequestContext::test_default())
                            .expect_err("oversized prompt must be rejected");
                        assert!(
                            matches!(err, ProviderError::InvalidRequest(_)),
                            "oversized prompt must map to InvalidRequest, got {err:?}",
                        );
                    }
                    None => {
                        // A stdin dialect has no argv cap — a large prompt is fine.
                        let big = "x".repeat(512 * 1024);
                        d.invocation(&ChatRequest::user(big), model, &RequestContext::test_default())
                            .expect("stdin dialect must accept a large prompt (no argv cap)");
                    }
                }
            }
        }
    };
}

cli_conformance_suite!(claude, scenarios::Claude);
cli_conformance_suite!(codex, scenarios::Codex);
cli_conformance_suite!(opencode, scenarios::OpenCode);
cli_conformance_suite!(antigravity, scenarios::Antigravity);
