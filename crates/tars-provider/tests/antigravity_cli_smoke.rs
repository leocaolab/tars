//! Live smoke test for the antigravity (`agy`) delegate against the real
//! binary. Drives the production path — [`AgentCliBackend`] +
//! [`AntigravityDialect`] + [`SharedCliRunner`] — exactly as the registry wires
//! it, so it exercises the real argv/spawn/parse, not a shortcut. **Requires**
//! the user has authenticated antigravity (`GEMINI_API_KEY` /
//! `ANTIGRAVITY_API_KEY` or its OAuth session).
//!
//! Always `#[ignore]`-d so a normal `cargo test` doesn't spawn the live CLI or
//! trigger billable inference. Run explicitly:
//!
//! ```bash
//! cargo test -p tars-provider --test antigravity_cli_smoke -- \
//!     --ignored --nocapture
//! ```
//!
//! KNOWN GAP (verified live 2026-08-15): `agy --model gemini-3.1-pro` rejects
//! the invocation unless a `--effort {low,high}` flag is also passed
//! (`invalid model selection … requires --effort`). `build_agy_argv` does NOT
//! emit `--effort`, so this test WILL fail against `gemini-3.1-pro` until the
//! dialect learns that flag. A bare `agy -p … --model gemini-3.1-pro --effort
//! high` DID return "hello from antigravity." when run by hand.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use tars_provider::backends::cli::{
    AgentCliBackend, AntigravityDialect, AntigravityEffort, SharedCliRunner,
};
use tars_provider::provider::LlmProvider;
use tars_types::{ChatEvent, ChatRequest, Pricing, ProviderProfile, RequestContext};

#[tokio::test]
#[ignore = "requires real agy CLI + antigravity auth; run with --ignored --nocapture"]
async fn antigravity_cli_say_hi_against_real_binary() {
    if which::which("agy").is_err() {
        panic!("`agy` not in PATH; install antigravity or skip this test");
    }

    // Build the SAME stack the registry's `ProviderConfig::Antigravity` arm
    // builds: an AgentCliBackend driven by an AntigravityDialect over the
    // shared SharedCliRunner.
    let dialect = Arc::new(AntigravityDialect::new(
        "agy".into(),
        Duration::from_secs(120),
        AntigravityEffort::High,
    ));
    let runner = Arc::new(SharedCliRunner::new(dialect.clone()));
    let caps = ProviderProfile::text_only_baseline(Pricing::default());
    let provider = Arc::new(AgentCliBackend::new(
        "antigravity_smoke".into(),
        caps,
        dialect,
        runner,
    ));

    let req = ChatRequest::user("Say exactly: hello from antigravity. Nothing else.");

    println!("\n── antigravity_cli smoke: spawning real `agy -p` ──");
    let mut stream = Arc::clone(&provider)
        .stream(req, "gemini-3.1-pro", RequestContext::test_default())
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
        full_text.to_lowercase().contains("hello")
            || full_text.to_lowercase().contains("antigravity"),
        "response should mention `hello` or `antigravity`; got: {full_text:?}",
    );
}
