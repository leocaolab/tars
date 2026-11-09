//! Replays the cassette the Python example recorded
//! (`examples/cassettes/schema-validation.cassette.json`), which the tars-node tests replay too, so
//! a recording made through `tars-py` is shown to replay here; it needs no model. The request must
//! be built exactly as the binding built it, or the replay misses: re-record with
//! `TARS_CASSETTE_RECORD=1` against the live provider and commit the new cassette, whose diff is
//! what gets reviewed.

use std::path::PathBuf;
use std::sync::Arc;
use tars_types::ProviderId;

use futures::StreamExt;
use serde::Deserialize;

use tars_llm::provider::LlmProvider;
use tars_llm::provider::backends::cassette::CassetteProvider;
use tars_types::{Caller, ChatEvent, ChatRequest, StructuredOutputMode};

const SYSTEM: &str = "You output ONLY a JSON object. No prose, no code fence.";
const USER: &str = "Rate the severity (0-10 integer) of this bug and summarize it \
in one sentence. Return an object with keys `severity` and `summary`.\n\nBUG: \
unwrap() on a None in the request handler panics the whole worker on malformed input.";

fn cassette_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/cassettes/schema-validation.cassette.json")
}

/// Returns the request exactly as the binding recorded it, so its fingerprint matches.
fn pinned_request() -> ChatRequest {
    let mut req = ChatRequest::user(USER).with_system(SYSTEM);
    req.max_output_tokens = Some(200);
    req
}
/// The model the cassette was recorded with; it is part of the fingerprint.
const PINNED_MODEL: &str = "qwen/qwen3-coder-30b";

async fn replay_text() -> String {
    let provider =
        CassetteProvider::replay_from_file(ProviderId::new("cassette_schema"), &cassette_path())
            .expect(
                "cassette file should exist (committed) — run the py example with \
                 TARS_CASSETTE_RECORD=1 to record it",
            );
    let stream = Arc::clone(&provider)
        .stream(
            pinned_request(),
            // `record_from = "qwen_coder_local"` in examples/tars.toml recorded this, and the
            // Python test names the same model.
            PINNED_MODEL,
            Caller::test_default(),
        )
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

    let review: Review =
        tars_utils::decode_json(&text, StructuredOutputMode::None).expect("pinned reply decodes");
    assert!(!review.summary.is_empty());
    assert_eq!(review.severity, 8, "pinned model reply severity");
}

#[tokio::test]
async fn cassette_replay_is_deterministic() {
    let a = replay_text().await;
    let b = replay_text().await;
    assert_eq!(a, b);
}
