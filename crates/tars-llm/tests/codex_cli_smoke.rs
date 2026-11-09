//! Runs [`AgentCliBackend`] against the real `codex`. It needs `codex login` on a ChatGPT
//! subscription and bills, so it is ignored by default:
//!
//! ```bash
//! cargo test -p tars-llm --test codex_cli_smoke -- \
//!     --ignored --nocapture
//! ```
//!
//! With `--nocapture` it prints every event, to compare with codex's own output.

use std::sync::Arc;
use tars_types::ProviderId;

use futures::StreamExt;

use tars_llm::provider::LlmProvider;
use tars_llm::provider::backends::codex_cli::CodexCliProviderBuilder;
use tars_types::{Caller, ChatEvent, ChatRequest};

#[tokio::test]
#[ignore = "requires real codex CLI + ChatGPT subscription; run with --ignored --nocapture"]
async fn codex_cli_say_hi_against_real_binary() {
    if which::which("codex").is_err() {
        panic!("`codex` not in PATH; install with `brew install codex` or skip this test",);
    }

    let provider = CodexCliProviderBuilder::new(ProviderId::new("codex_smoke"))
        // codex's agent loop is slow to start.
        .timeout(std::time::Duration::from_secs(120))
        .build();

    // ChatGPT subscriptions can use specific model overrides (gpt-5.5,
    // gpt-5.4, gpt-5.3-codex, etc. — vary by account); the bare names
    // `gpt-5` / `gpt-5-codex` are gated to API accounts only and 400
    // with `model is not supported when using Codex with a ChatGPT
    // account`. gpt-5.5 works on the test account.
    let req = ChatRequest::user("Say exactly: hello from codex. Nothing else.");

    println!("\n── codex_cli smoke: spawning real `codex exec` ──");
    let mut stream = Arc::clone(&provider)
        .stream(req, "test-model", Caller::test_default())
        .await
        .expect("provider stream() should succeed");

    let mut event_count = 0;
    let mut text_chunks: Vec<String> = Vec::new();
    let mut thinking_chunks: Vec<String> = Vec::new();
    let mut saw_finished = false;
    while let Some(ev) = stream.next().await {
        event_count += 1;
        match ev {
            Ok(ChatEvent::Degraded { unsent }) => {
                println!("[evt {event_count:>2}] Degraded    unsent={unsent:?}");
            }
            Ok(ChatEvent::Started { actual_model, .. }) => {
                println!("[evt {event_count:>2}] Started     model={actual_model:?}");
            }
            Ok(ChatEvent::Delta { text }) => {
                println!("[evt {event_count:>2}] Delta       text={text:?}");
                text_chunks.push(text);
            }
            Ok(ChatEvent::ThinkingDelta { text }) => {
                println!("[evt {event_count:>2}] Thinking    text={text:?}");
                thinking_chunks.push(text);
            }
            Ok(ChatEvent::ToolCallStart { id, name, .. }) => {
                println!("[evt {event_count:>2}] ToolStart   id={id} name={name}");
            }
            Ok(ChatEvent::ToolCallArgsDelta { index, args_delta }) => {
                println!("[evt {event_count:>2}] ToolArgs    idx={index} delta={args_delta:?}");
            }
            Ok(ChatEvent::ToolCallEnd { id, .. }) => {
                println!("[evt {event_count:>2}] ToolEnd     id={id}");
            }
            Ok(ChatEvent::UsageProgress { usage }) => {
                println!("[evt {event_count:>2}] UsageProg   {usage:?}");
            }
            Ok(ChatEvent::Measured { telemetry }) => {
                println!("[evt {event_count:>2}] Measured    {telemetry:?}");
            }
            Ok(ChatEvent::Validated { summary }) => {
                println!("[evt {event_count:>2}] Validated   {summary:?}");
            }
            Ok(ChatEvent::Finished { stop_reason, usage }) => {
                println!(
                    "[evt {event_count:>2}] Finished    stop={stop_reason:?} \
                     in={} out={} cached={} thinking={}",
                    usage.unwrap().input_tokens,
                    usage.unwrap().output_tokens,
                    usage.unwrap().cached_input_tokens,
                    usage.unwrap().thinking_tokens,
                );
                saw_finished = true;
            }
            Err(e) => {
                panic!("[evt {event_count:>2}] ERROR       {e:?}");
            }
        }
    }

    let full_text = text_chunks.concat();
    let full_thinking = thinking_chunks.concat();

    println!("\n── final result ──");
    println!("text     = {full_text:?}");
    if !full_thinking.is_empty() {
        println!("thinking = {full_thinking:?}");
    }
    println!("events   = {event_count}");

    assert!(saw_finished, "stream must end with Finished");
    assert!(
        !full_text.is_empty(),
        "should have received some text from codex"
    );
    // The model does not repeat the words exactly.
    assert!(
        full_text.to_lowercase().contains("hello") || full_text.to_lowercase().contains("codex"),
        "response should mention `hello` or `codex`; got: {full_text:?}",
    );
}
