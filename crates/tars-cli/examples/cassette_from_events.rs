//! Export a deterministic-replay cassette from a tars event store.
//!
//! The event store (`<dir>/pipeline_events.db` + `bodies.db`) already records
//! every real LLM call: each `llm_call_finished` event carries a
//! `request_ref`/`response_ref` (body hashes) and the raw bodies are the
//! serialized `ChatRequest` / `ChatResponse`. This turns that into a
//! `{ request_fingerprint : [ChatEvent] }` cassette the `CassetteProvider`
//! replays — so a real run becomes a deterministic test with NO re-recording.
//!
//! Usage:
//!   cargo run -p tars-cli --example cassette_from_events -- <event-store-dir> <out.json> [provider_id]

use std::collections::HashMap;

use rusqlite::Connection;
use tars_provider::backends::cassette::request_fingerprint;
use tars_types::{CacheHitInfo, ChatRequest, ChatResponse};

/// A `body_hash` is stored in the event payload as a JSON array of bytes.
fn hash_bytes(v: &serde_json::Value) -> Vec<u8> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64()).map(|x| x as u8).collect())
        .unwrap_or_default()
}

fn body(bo: &Connection, hash: &[u8]) -> Option<String> {
    bo.query_row("SELECT body FROM bodies WHERE body_hash = ?1", [hash], |r| {
        let b: Vec<u8> = r.get(0)?;
        Ok(String::from_utf8_lossy(&b).into_owned())
    })
    .ok()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: cassette_from_events <event-store-dir> <out.json> [provider_id]");
        std::process::exit(2);
    }
    let dir = &args[1];
    let out = &args[2];
    let provider_filter = args.get(3).cloned();

    let ev = Connection::open(format!("{dir}/pipeline_events.db")).expect("open pipeline_events.db");
    let bo = Connection::open(format!("{dir}/bodies.db")).expect("open bodies.db");

    let mut cassette: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    let (mut total, mut exported, mut skipped) = (0usize, 0usize, 0usize);

    let mut stmt = ev
        .prepare("SELECT payload_json FROM pipeline_events WHERE event_type = 'llm_call_finished'")
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .expect("query");

    for row in rows {
        total += 1;
        let payload: serde_json::Value = match serde_json::from_slice(&row.unwrap()) {
            Ok(p) => p,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if let Some(f) = &provider_filter {
            if payload["provider_id"].as_str() != Some(f.as_str()) {
                continue;
            }
        }
        let resp_ref = &payload["response_ref"];
        if resp_ref.is_null() {
            skipped += 1; // a crashed/incomplete call has no response to replay
            continue;
        }
        let req_body = body(&bo, &hash_bytes(&payload["request_ref"]["body_hash"]));
        let resp_body = body(&bo, &hash_bytes(&resp_ref["body_hash"]));
        let (Some(req_body), Some(resp_body)) = (req_body, resp_body) else {
            skipped += 1;
            continue;
        };
        let req: ChatRequest = match serde_json::from_str(&req_body) {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let resp: ChatResponse = match serde_json::from_str(&resp_body) {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        // The cassette key is computed the SAME way replay computes it (on the
        // live ChatRequest, after volatile-path normalization), so the recorded
        // call matches when the test reproduces the same request.
        let key = request_fingerprint(&req);
        let events: Vec<serde_json::Value> = resp
            .into_events(CacheHitInfo::default())
            .into_iter()
            .map(|e| serde_json::to_value(e).expect("ChatEvent serializes"))
            .collect();
        cassette.insert(key, events);
        exported += 1;
    }

    std::fs::write(out, serde_json::to_string_pretty(&cassette).unwrap()).expect("write cassette");
    eprintln!(
        "events={total} exported={exported} skipped={skipped} unique_keys={} -> {out}",
        cassette.len()
    );
}
