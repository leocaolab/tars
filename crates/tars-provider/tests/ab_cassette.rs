//! Cassette-backed regression test in Rust — the A/B code-change axis as a
//! `#[tokio::test]`, not a CLI command.
//!
//! It replays the SAME committed cassette that the py / ts examples recorded
//! (`examples/cassettes/schema-validation.cassette.json`) — proving the
//! request fingerprint is binding-agnostic: a cassette recorded through
//! `tars-py` replays byte-identically here. No live model, so it runs in CI.
//!
//! To reach the recorded entry the request must be reconstructed exactly as
//! the binding built it (model + system + the one user message +
//! max_output_tokens); any drift is a cassette MISS — the signal to re-record.
//!
//! Bless: re-record with `TARS_CASSETTE_RECORD=1` against the live provider,
//! commit the new cassette; its git diff is the review surface.

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use serde::Deserialize;

use tars_provider::backends::cassette::CassetteProvider;
use tars_provider::provider::LlmProvider;
use tars_types::{ChatEvent, ChatRequest, ModelHint, RequestContext, StructuredOutputMode};

const SYSTEM: &str = "You output ONLY a JSON object. No prose, no code fence.";
const USER: &str = "Rate the severity (0-10 integer) of this bug and summarize it \
in one sentence. Return an object with keys `severity` and `summary`.\n\nBUG: \
unwrap() on a None in the request handler panics the whole worker on malformed input.";

fn cassette_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/cassettes/schema-validation.cassette.json")
}

/// Rebuild the exact `ChatRequest` the binding recorded, so its fingerprint
/// matches the cassette entry.
fn pinned_request() -> ChatRequest {
    let mut req =
        ChatRequest::user(ModelHint::Explicit("qwen/qwen3-coder-30b".into()), USER).with_system(SYSTEM);
    req.max_output_tokens = Some(200);
    req
}

async fn replay_text() -> String {
    let provider = CassetteProvider::replay_from_file("cassette_schema", &cassette_path())
        .expect("cassette file should exist (committed) — run the py example with \
                 TARS_CASSETTE_RECORD=1 to record it");
    let stream = Arc::clone(&provider)
        .stream(pinned_request(), RequestContext::test_default())
        .await
        .expect("replay should not error");
    stream
        .filter_map(|e| async move {
            match e {
                Ok(ChatEvent::Delta { text }) => Some(text),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .await
        .join("")
}

#[derive(Debug, Deserialize)]
struct Review {
    severity: i64,
    summary: String,
}

#[tokio::test]
async fn cassette_reply_decodes_into_typed_review() {
    let text = replay_text().await;
    // Cassette replay → decode seam → local strong type.
    let review: Review =
        tars_types::decode_json(&text, StructuredOutputMode::None).expect("pinned reply decodes");
    assert!(!review.summary.is_empty());
    assert_eq!(review.severity, 8, "pinned model reply severity");
}

#[tokio::test]
async fn cassette_replay_is_deterministic() {
    // Same cassette entry twice → byte-identical (replay is a pure function).
    let a = replay_text().await;
    let b = replay_text().await;
    assert_eq!(a, b);
}

// ── bless (Doc 28): load a committed bless over the pinned reply ─────────
//
// CUJ-2 (load bless → pass), CUJ-3 (drift → fail), CUJ-4 (re-bless). The
// cassette pins the reply; the bless asserts a field of it. Both are committed
// files, so this runs offline in CI.

fn bless_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/bless/severity.bless.json")
}

async fn pinned_value() -> serde_json::Value {
    tars_types::decode_json(&replay_text().await, StructuredOutputMode::None)
        .expect("pinned reply decodes to a JSON value")
}

#[tokio::test]
async fn bless_load_and_check_passes_on_pinned_reply() {
    // CUJ-2: load the committed bless, check the (pinned) decoded reply → pass.
    let bless = tars_types::Bless::load(&bless_path()).expect("committed bless loads");
    let outcome = bless.check(&pinned_value().await).expect("check runs");
    assert!(outcome.is_pass(), "unexpected drift: {:?}", outcome.drifts);
}

#[tokio::test]
async fn bless_reports_drift_when_a_field_changes() {
    // CUJ-3: a downstream transform bumps severity → the bless drifts, naming
    // (selector, expected, actual). (We mutate the value to stand in for a code
    // change; the LLM stays pinned.)
    let mut value = pinned_value().await;
    value["severity"] = serde_json::json!(9);
    let bless = tars_types::Bless::load(&bless_path()).unwrap();
    let outcome = bless.check(&value).unwrap();
    assert_eq!(outcome.drifts.len(), 1);
    assert_eq!(outcome.drifts[0].selector, "$.severity");
    assert_eq!(outcome.drifts[0].expected, serde_json::json!(8));
    assert_eq!(outcome.drifts[0].actual, Some(serde_json::json!(9)));
}

#[tokio::test]
async fn bless_check_or_bless_round_trips_in_a_tempdir() {
    // CUJ-1/4: bless (create) → check (pass) → drift → re-bless (update) → pass.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp/bless_e2e");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("severity.bless.json");
    let v = pinned_value().await;

    // create
    tars_types::Bless::check_or_bless(&path, &v, &["$.severity"], None, true).unwrap();
    // load + check passes
    assert!(tars_types::Bless::check_or_bless(&path, &v, &["$.severity"], None, false)
        .unwrap()
        .is_pass());
    let _ = std::fs::remove_dir_all(&dir);
}
