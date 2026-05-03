//! Live smoke test for [`CodexCliProvider`] against the real `codex`
//! binary. **Requires** the user has already run `codex login` and has
//! a valid `~/.codex/auth.json` (ChatGPT subscription).
//!
//! Always `#[ignore]`-d so a normal `cargo test` doesn't trigger
//! billable inference. Run explicitly:
//!
//! ```bash
//! cargo test -p tars-provider --test codex_cli_smoke -- \
//!     --ignored --nocapture
//! ```
//!
//! The `--nocapture` is what makes this test useful — it dumps every
//! ChatEvent so you can eyeball the mapping against codex's actual
//! JSONL output.

use std::sync::Arc;

use futures::StreamExt;

use tars_provider::backends::codex_cli::CodexCliProviderBuilder;
use tars_provider::provider::LlmProvider;
use tars_types::{ChatEvent, ChatRequest, ModelHint, RequestContext};

#[tokio::test]
#[ignore = "requires real codex CLI + ChatGPT subscription; run with --ignored --nocapture"]
async fn codex_cli_say_hi_against_real_binary() {
    if which::which("codex").is_err() {
        panic!(
            "`codex` not in PATH; install with `brew install codex` or skip this test",
        );
    }

    let provider = CodexCliProviderBuilder::new("codex_smoke")
        // Default sandbox=ReadOnly; default skip_git_repo_check=true.
        // 2-minute timeout — "say hi" should be fast but codex's agent
        // loop has its own startup overhead.
        .timeout(std::time::Duration::from_secs(120))
        .build();

    // ChatGPT subscriptions can use specific model overrides (gpt-5.5,
    // gpt-5.4, gpt-5.3-codex, etc. — vary by account); the bare names
    // `gpt-5` / `gpt-5-codex` are gated to API accounts only and 400
    // with `model is not supported when using Codex with a ChatGPT
    // account`. gpt-5.5 works on the test account.
    let req = ChatRequest::user(
        ModelHint::Explicit("gpt-5.5".into()),
        "Say exactly: hello from codex. Nothing else.",
    );

    println!("\n── codex_cli smoke: spawning real `codex exec` ──");
    let mut stream = Arc::clone(&provider)
        .stream(req, RequestContext::test_default())
        .await
        .expect("provider stream() should succeed");

    let mut event_count = 0;
    let mut text_chunks: Vec<String> = Vec::new();
    let mut thinking_chunks: Vec<String> = Vec::new();
    let mut saw_finished = false;
    while let Some(ev) = stream.next().await {
        event_count += 1;
        match ev {
            Ok(ChatEvent::Started { actual_model, .. }) => {
                println!("[evt {event_count:>2}] Started     model={actual_model}");
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
            Ok(ChatEvent::UsageProgress { partial }) => {
                println!("[evt {event_count:>2}] UsageProg   {partial:?}");
            }
            Ok(ChatEvent::Finished { stop_reason, usage }) => {
                println!(
                    "[evt {event_count:>2}] Finished    stop={stop_reason:?} \
                     in={} out={} cached={} thinking={}",
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_input_tokens,
                    usage.thinking_tokens,
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
    assert!(!full_text.is_empty(), "should have received some text from codex");
    // Don't pin exact text — model isn't deterministic enough at the
    // word level even with our prompt. Just sanity-check it's
    // non-empty and looks vaguely like an answer.
    assert!(
        full_text.to_lowercase().contains("hello") || full_text.to_lowercase().contains("codex"),
        "response should mention `hello` or `codex`; got: {full_text:?}",
    );
}
