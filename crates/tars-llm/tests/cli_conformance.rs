//! Checks what every CLI dialect must do alike, with fake runners: its prompt channel, output
//! mode and framing; the environment it strips (upper-cased, and nothing else, so antigravity and
//! opencode strip nothing); the caller's deadline winning over the configured timeout (an expired
//! one gives zero), which only this layer can enforce because a subprocess outlives its future;
//! a well-formed payload read as `Started`, one `Delta` and `Finished(EndTurn)` whatever the
//! framing; a dead CLI's error passed through; and a prompt over an argument dialect's cap
//! refused. Each dialect's argv and its own parsing are tested in its module. A new dialect needs
//! a `CliScenarios` impl and one `cli_conformance_suite!` line.

#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};

use tars_llm::ProviderError;
use tars_llm::provider::LlmProvider;
use tars_llm::provider::backends::cli::{
    AgentCliBackend, AntigravityDialect, AntigravityEffort, ClaudeCliDialect, ClaudeToolAccess,
    CliDialect, CodexCliDialect, CodexSandbox, OpenCodeDialect, OutputFraming, OutputMode,
    PromptChannel, RunFailure, RunOutput, SubprocessInvocation, SubprocessRunner,
};
use tars_types::{Caller, ChatEvent, ChatRequest, Pricing, ProviderProfile, StopReason};

/// The configured timeout of every scenario; an odd value no deadline produces by accident.
const CONFIG_TIMEOUT: Duration = Duration::from_secs(137);

const ANSWER: &str = "conformance answer";

// The fake runners answer with the payload the real runner would frame for the dialect, or a
// failure.

struct OkRunner {
    payload: Value,
}

#[async_trait]
impl SubprocessRunner for OkRunner {
    async fn run(&self, _inv: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
        Ok(RunOutput {
            payload: self.payload.clone(),
            exchanged: Default::default(),
        })
    }
}

struct DeadRunner;

#[async_trait]
impl SubprocessRunner for DeadRunner {
    async fn run(&self, _inv: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
        Err(RunFailure::before_exchange(
            ProviderError::CliSubprocessDied {
                exit_code: Some(0),
                stderr: "conformance: simulated dead subprocess".into(),
            },
        ))
    }
}

fn backend(
    dialect: Arc<dyn CliDialect>,
    runner: Arc<dyn SubprocessRunner>,
) -> Arc<AgentCliBackend> {
    Arc::new(AgentCliBackend::new(
        tars_types::ProviderId::new("cli_conf"),
        ProviderProfile::text_only_baseline(Pricing::default()),
        dialect,
        runner,
    ))
}

/// Returns the lines framed as the shared runner frames a `JsonLinesArray` dialect's output.
fn jsonl(lines: &[&str]) -> Value {
    Value::Array(lines.iter().map(|l| Value::String((*l).into())).collect())
}

mod scenarios {
    use super::*;

    pub struct Claude;
    impl Claude {
        pub fn dialect() -> Arc<dyn CliDialect> {
            Arc::new(ClaudeCliDialect::new(
                "claude".into(),
                CONFIG_TIMEOUT,
                ClaudeToolAccess::Disabled,
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
            None
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
                CodexSandbox::ReadOnly,
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
            None
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
            &[]
        }
        pub fn prompt_cap() -> Option<usize> {
            // Linux `MAX_ARG_STRLEN` (128 KiB) less the terminating NUL.
            Some(128 * 1024 - 1)
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
            &[]
        }
        pub fn prompt_cap() -> Option<usize> {
            // Linux `MAX_ARG_STRLEN` (128 KiB) less the terminating NUL.
            Some(128 * 1024 - 1)
        }
        pub fn success_payload() -> Value {
            // agy ends its answer with a newline.
            Value::String(format!("{ANSWER}\n"))
        }
    }
}

macro_rules! cli_conformance_suite {
    ($name:ident, $scenarios:ty) => {
        mod $name {
            use super::*;

            #[test]
            fn declares_channel_mode_framing() {
                let d = <$scenarios>::dialect();
                let (channel, mode, framing) = <$scenarios>::declarations();
                assert_eq!(d.prompt_channel(), channel, "prompt channel");
                assert_eq!(d.output_mode(), mode, "output mode");
                assert_eq!(d.output_framing(), framing, "output framing");
            }

            #[test]
            fn strips_exactly_the_declared_auth_env() {
                let d = <$scenarios>::dialect();
                let inv = d
                    .invocation(
                        &ChatRequest::user("hi"),
                        <$scenarios>::model(),
                        &Caller::test_default(),
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
                assert_eq!(
                    inv.stripped_env.len(),
                    expected.len(),
                    "stripped_env must equal exactly the declared key set, got {:?}",
                    inv.stripped_env,
                );
            }

            #[test]
            fn caller_deadline_wins_over_configured_timeout() {
                let d = <$scenarios>::dialect();
                let model = <$scenarios>::model();

                let inv = d
                    .invocation(&ChatRequest::user("x"), model, &Caller::test_default())
                    .unwrap();
                assert_eq!(
                    inv.timeout, CONFIG_TIMEOUT,
                    "no deadline ⇒ configured timeout is the default",
                );

                let mut ctx = Caller::test_default();
                ctx.deadline = Some(Instant::now() + Duration::from_secs(600));
                let inv = d.invocation(&ChatRequest::user("x"), model, &ctx).unwrap();
                assert!(
                    inv.timeout > Duration::from_secs(500) && inv.timeout <= Duration::from_secs(600),
                    "longer deadline must set the budget (not clamp to config), got {:?}",
                    inv.timeout,
                );

                let mut ctx = Caller::test_default();
                ctx.deadline = Some(Instant::now() + Duration::from_secs(5));
                let inv = d.invocation(&ChatRequest::user("x"), model, &ctx).unwrap();
                assert!(inv.timeout <= Duration::from_secs(5), "got {:?}", inv.timeout);

                let mut ctx = Caller::test_default();
                ctx.deadline = Some(Instant::now() - Duration::from_secs(1));
                let inv = d.invocation(&ChatRequest::user("x"), model, &ctx).unwrap();
                assert_eq!(inv.timeout, Duration::ZERO, "expired deadline ⇒ ZERO budget");
            }

            #[tokio::test]
            async fn success_payload_yields_started_delta_finished() {
                let backend = backend(
                    <$scenarios>::dialect(),
                    Arc::new(OkRunner { payload: <$scenarios>::success_payload() }),
                );
                let model = <$scenarios>::model();

                let events: Vec<ChatEvent> = Arc::clone(&backend)
                    .stream(ChatRequest::user("hi"), model, Caller::test_default())
                    .await
                    .expect("stream should open")
                    .map(|e| e.expect("no error event"))
                    .collect()
                    .await;


                assert!(
                    matches!(&events[0], ChatEvent::Started { actual_model, .. } if actual_model.as_deref() == Some(model)),
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

            #[tokio::test]
            async fn dead_subprocess_error_propagates() {
                let backend = backend(<$scenarios>::dialect(), Arc::new(DeadRunner));
                let err = backend
                    .complete(
                        ChatRequest::user("hi"),
                        <$scenarios>::model(),
                        Caller::test_default(),
                    )
                    .await
                    .expect_err("a dead subprocess must surface an error");
                assert!(
                    matches!(err, ProviderError::CliSubprocessDied { .. }),
                    "dead subprocess must map to CliSubprocessDied, got {err:?}",
                );
            }

            #[test]
            fn prompt_size_cap_is_enforced() {
                let d = <$scenarios>::dialect();
                let model = <$scenarios>::model();
                match <$scenarios>::prompt_cap() {
                    Some(cap) => {
                        // The rendered prompt adds a few bytes of role framing to the user text.
                        d.invocation(&ChatRequest::user("x".repeat(cap - 64)), model, &Caller::test_default())
                            .expect("a prompt under the cap must be accepted");
                        let big = "x".repeat(cap + 1);
                        let err = d
                            .invocation(&ChatRequest::user(big), model, &Caller::test_default())
                            .expect_err("oversized prompt must be rejected");
                        assert!(
                            matches!(err, ProviderError::InvalidRequest(_)),
                            "oversized prompt must map to InvalidRequest, got {err:?}",
                        );
                    }
                    None => {
                        let big = "x".repeat(512 * 1024);
                        d.invocation(&ChatRequest::user(big), model, &Caller::test_default())
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
