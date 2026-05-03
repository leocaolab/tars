//! Live smoke test for [`GeminiCliProvider`] against the real `gemini`
//! binary. **Requires** the user has already authenticated their
//! Google account (`gemini` walks you through OAuth on first run).
//!
//! Always `#[ignore]`-d so a normal `cargo test` doesn't trigger
//! billable inference. Run explicitly:
//!
//! ```bash
//! cargo test -p tars-provider --test gemini_cli_smoke -- \
//!     --ignored --nocapture
//! ```

use std::sync::Arc;

use futures::StreamExt;

use tars_provider::backends::gemini_cli::GeminiCliProviderBuilder;
use tars_provider::provider::LlmProvider;
use tars_types::{ChatEvent, ChatRequest, ModelHint, RequestContext};

#[tokio::test]
#[ignore = "requires real gemini CLI + Google account; run with --ignored --nocapture"]
async fn gemini_cli_say_hi_against_real_binary() {
    if which::which("gemini").is_err() {
        panic!("`gemini` not in PATH; install Gemini CLI or skip this test");
    }

    let provider = GeminiCliProviderBuilder::new("gemini_smoke")
        .timeout(std::time::Duration::from_secs(120))
        .build();

    let req = ChatRequest::user(
        ModelHint::Explicit("gemini-2.5-pro".into()),
        "Say exactly: hello from gemini. Nothing else.",
    );

    println!("\n── gemini_cli smoke: spawning real `gemini` ──");
    let mut stream = Arc::clone(&provider)
        .stream(req, RequestContext::test_default())
        .await
        .expect("provider stream() should succeed");

    let mut event_count = 0;
    let mut text_chunks: Vec<String> = Vec::new();
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
        full_text.to_lowercase().contains("hello") || full_text.to_lowercase().contains("gemini"),
        "response should mention `hello` or `gemini`; got: {full_text:?}",
    );
}
