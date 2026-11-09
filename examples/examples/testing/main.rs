//! Testing an agent with a mocked LLM — deterministic + fast.
//!
//! The point: your agent's *logic* — prompt assembly, response parsing,
//! branching, retries, error handling — should be testable without a
//! network call, an API key, or a nondeterministic model. tars ships
//! [`MockProvider`] for exactly this. You script the LLM's answer, run
//! the agent against it, and assert on BOTH its output AND the requests
//! it sent.
//!
//! Two properties you get:
//!   - **Deterministic** — the model's reply is whatever you scripted,
//!     identical on every run, so a failing test means a real bug.
//!   - **Fast** — no HTTP, no SSE, no rate limits. Each call is an
//!     in-memory event replay (sub-millisecond), so a whole suite runs
//!     in the time one real call would spend on the handshake.
//!
//! The seam that makes it work: an agent that holds an
//! `LlmService` (not a concrete provider) can have a
//! `MockProvider` dropped in for tests and a real pipeline in prod.
//!
//! Run:
//! ```bash
//! cargo run -p tars-examples --example testing
//! ```
//! (In your crate these would be `#[test]`s; here they're a runnable
//! `main` that panics on the first failed assertion.)

use std::sync::Arc;
use tars_types::ProviderId;

use tars_llm::LlmError;
use tars_llm::{CannedResponse, MockProvider};
use tars_llm::{LlmService, complete_async};
use tars_types::{Caller, ChatRequest};

/// An example's tenant: a real deployment passes whose work the run is.
const EXAMPLE_TENANT: &str = "example";

// ── The agent under test ────────────────────────────────────────────
//
// A tiny reviewer: classify a code snippet in one word. It depends only
// on an injected `LlmService` — that injection is the whole
// reason it's testable. Real agents add prompt templates, tool loops,
// retries; the testing pattern is identical.

struct ReviewAgent {
    llm: LlmService,
}

impl ReviewAgent {
    /// Send one classification prompt; return the model's verdict
    /// (trimmed). Propagates provider failures so callers — and tests —
    /// can react to them.
    async fn classify(&self, code: &str) -> Result<String, LlmError> {
        let prompt = format!("Reply with exactly one word — ok, bug, or unsure:\n{code}");
        let req = ChatRequest::user(prompt);
        let resp = complete_async(
            self.llm.clone(),
            req,
            Caller::new(
                tars_types::TenantId::new(EXAMPLE_TENANT),
                tars_types::TraceId::new("testing"),
            ),
        )
        .await?;
        Ok(resp.text.trim().to_string())
    }
}

/// Wrap a `MockProvider` as the `LlmService` an agent expects.
/// (In prod this is a full `Pipeline`; for tests the bare provider
/// service is enough — no retry/cache/telemetry layers needed.)
fn agent_with(mock: &Arc<MockProvider>) -> ReviewAgent {
    ReviewAgent {
        llm: LlmService::of(mock.clone(), "demo-model"),
    }
}

// ── 1. Deterministic output + request inspection ────────────────────

async fn deterministic_output() {
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("bug"));
    let agent = agent_with(&mock);

    // Scripted answer → identical output every run, no network.
    assert_eq!(agent.classify("fn f() { panic!() }").await.unwrap(), "bug");
    assert_eq!(agent.classify("fn f() { panic!() }").await.unwrap(), "bug");

    // You can also assert on what the agent SENT the model — that its
    // prompt assembly is correct, not just its output parsing.
    assert_eq!(mock.call_count(), 2);
    let sent = format!("{:?}", mock.history_snapshot());
    assert!(
        sent.contains("exactly one word"),
        "prompt should carry the instruction"
    );

    println!("✓ deterministic_output — same script, same answer, prompt verified");
}

// ── 2. Error-path testing (no flaky provider required) ──────────────

async fn error_injection() {
    // Script a provider failure deterministically, instead of waiting
    // for a real 500 / timeout to test your error handling.
    let mock = MockProvider::new(
        ProviderId::new("mock"),
        CannedResponse::Error("provider exploded".into()),
    );
    let agent = agent_with(&mock);

    let result = agent.classify("anything").await;
    assert!(
        result.is_err(),
        "scripted provider error must surface to the caller"
    );

    println!("✓ error_injection — failure path exercised without a flaky network");
}

// ── 3. Multi-turn: vary the script across calls ─────────────────────

async fn multi_turn() {
    // `set_response` swaps the canned answer between calls — pin down an
    // agent that behaves differently turn to turn (e.g. unsure → decisive
    // after a sharper follow-up), still fully deterministic.
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("unsure"));
    let agent = agent_with(&mock);

    assert_eq!(agent.classify("ambiguous code").await.unwrap(), "unsure");

    mock.set_response(CannedResponse::text("bug"));
    assert_eq!(
        agent.classify("ambiguous code, second look").await.unwrap(),
        "bug"
    );

    assert_eq!(mock.call_count(), 2);

    println!("✓ multi_turn — scripted turn-by-turn behaviour, deterministic");
}

#[tokio::main]
async fn main() {
    deterministic_output().await;
    error_injection().await;
    multi_turn().await;

    println!(
        "\nAll mock-LLM agent tests passed — no network, no API key, instant + repeatable.\n\
         Pattern: give your agent an `LlmService` seam; inject `MockProvider`\n\
         in tests, a real `Pipeline` in prod. For a per-call scripted sequence (a\n\
         different reply each turn inside one agent loop, plus tool calls), see the\n\
         `ScriptedProvider` in examples/multi_step_with_tools.rs."
    );
}
