//! Runs [`AgentCliBackend`] against the real `claude`. It needs a logged-in Claude Code
//! subscription (run `claude` once) and bills, so it is ignored by default:
//!
//! ```bash
//! cargo test -p tars-llm --test claude_cli_smoke -- \
//!     --ignored --nocapture
//! ```

use std::sync::Arc;
use tars_types::ProviderId;

use futures::StreamExt;

use tars_llm::provider::LlmProvider;
use tars_llm::provider::backends::claude_cli::ClaudeCliProviderBuilder;
use tars_types::{Caller, ChatEvent, ChatRequest};

#[tokio::test]
#[ignore = "requires real claude CLI + Claude Code subscription; run with --ignored --nocapture"]
async fn claude_cli_say_hi_against_real_binary() {
    if which::which("claude").is_err() {
        panic!("`claude` not in PATH; install Claude Code or skip this test");
    }

    let provider = ClaudeCliProviderBuilder::new(ProviderId::new("claude_smoke"))
        .timeout(std::time::Duration::from_secs(120))
        .build();

    // `haiku` is the alias to the latest Haiku model; `sonnet` and `opus` work too, as far as
    // the subscription tier allows. claude refuses a name it does not know.
    let req = ChatRequest::user("Say exactly: hello from claude. Nothing else.");

    println!("\n── claude_cli smoke: spawning real `claude -p` ──");
    let mut stream = Arc::clone(&provider)
        .stream(req, "haiku", Caller::test_default())
        .await
        .expect("provider stream() should succeed");

    let mut event_count = 0;
    let mut text_chunks: Vec<String> = Vec::new();
    let mut saw_finished = false;
    while let Some(ev) = stream.next().await {
        event_count += 1;
        match ev {
            Ok(ChatEvent::Started { actual_model, .. }) => {
                println!("[evt {event_count:>2}] Started     model={actual_model:?}");
            }
            Ok(ChatEvent::Delta { text }) => {
                println!("[evt {event_count:>2}] Delta       text={text:?}");
                text_chunks.push(text);
            }
            Ok(ChatEvent::ThinkingDelta { text }) => {
                println!("[evt {event_count:>2}] Thinking    text={text:?}");
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
            Ok(other) => println!("[evt {event_count:>2}] {other:?}"),
            Err(e) => panic!("[evt {event_count:>2}] ERROR       {e:?}"),
        }
    }

    let full_text = text_chunks.concat();
    println!("\n── final result ──");
    println!("text     = {full_text:?}");
    println!("events   = {event_count}");

    assert!(saw_finished, "stream must end with Finished");
    assert!(!full_text.is_empty(), "should have received some text");
    assert!(
        full_text.to_lowercase().contains("hello") || full_text.to_lowercase().contains("claude"),
        "response should mention `hello` or `claude`; got: {full_text:?}",
    );
}
