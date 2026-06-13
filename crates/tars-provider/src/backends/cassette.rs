//! Cassette provider — deterministic LLM replay for testing (Doc 18 §5a).
//!
//! Pins the LLM so a code-change A/B isolates the CODE, not model noise. Two
//! modes, ONE request-fingerprint function (so record and replay agree):
//!
//!   - **record**: wrap a real provider, pass its responses through, and capture
//!     `(request fingerprint → full event sequence)` into a cassette file.
//!   - **replay**: serve the recorded events for a matching request; a **miss
//!     is a signal** (an input the recording didn't cover — usually a prompt that
//!     changed) and surfaces as a provider error, never a silent wrong answer.
//!
//! The fingerprint is a stable hash of the serialized `ChatRequest` (model +
//! system + messages + tools + schema) — `ChatRequest: Serialize` — so the same
//! logical request maps to the same key at record and replay time.
//!
//! The cassette stores the **whole `Vec<ChatEvent>`** per request, not just the
//! text — so it replays tool calls (`ToolCallStart`/`ToolCallEnd`) verbatim and
//! can freeze a white-box AGENT (fixer) tool loop, not only a text critic. A
//! multi-turn agent records N (request → events) pairs in one session; each
//! later turn's request (carrying the prior tool results) hashes to its own key
//! and replays in turn.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{stream, StreamExt};

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, Pricing, ProviderError, ProviderId, RequestContext,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// The recorded response for one request: the exact successful event sequence.
type Recording = Vec<ChatEvent>;

/// Stable fingerprint of a request's deterministic content. Record and replay
/// MUST compute it identically — both call this on the live `ChatRequest`.
pub fn request_fingerprint(req: &ChatRequest) -> String {
    let canon =
        serde_json::to_string(req).unwrap_or_else(|_| format!("{:?}", req.model.label()));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut h);
    format!("{:016x}", h.finish())
}

enum Mode {
    /// Pass through `inner`, capturing each (fingerprint → events) into
    /// `captured`. `flush_path` (if set) is written after every capture.
    Record {
        inner: Arc<dyn LlmProvider>,
        captured: Mutex<HashMap<String, Recording>>,
        flush_path: Option<PathBuf>,
    },
    /// Serve recorded events by fingerprint; a miss is an error (signal).
    Replay { cassette: HashMap<String, Recording> },
}

pub struct CassetteProvider {
    id: ProviderId,
    capabilities: Capabilities,
    mode: Mode,
}

impl CassetteProvider {
    /// Replay from a loaded cassette (fingerprint → recorded event sequence).
    pub fn replay(id: impl Into<ProviderId>, cassette: HashMap<String, Recording>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            capabilities: Capabilities::text_only_baseline(Pricing::default()),
            mode: Mode::Replay { cassette },
        })
    }

    /// Record by wrapping a real provider; flush the captured map with `take`.
    pub fn record(id: impl Into<ProviderId>, inner: Arc<dyn LlmProvider>) -> Arc<Self> {
        Self::record_to(id, inner, None)
    }

    /// Record + flush to `flush_path` (if set) after every captured response.
    pub fn record_to(
        id: impl Into<ProviderId>,
        inner: Arc<dyn LlmProvider>,
        flush_path: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            capabilities: inner.capabilities().clone(),
            mode: Mode::Record {
                inner,
                captured: Mutex::new(HashMap::new()),
                flush_path,
            },
        })
    }

    /// Load a cassette file (`{fingerprint: [events]}` JSON) for replay.
    pub fn replay_from_file(
        id: impl Into<ProviderId>,
        path: &std::path::Path,
    ) -> std::io::Result<Arc<Self>> {
        let raw = std::fs::read_to_string(path)?;
        let cassette: HashMap<String, Recording> = serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self::replay(id, cassette))
    }

    /// Drain everything captured so far (record mode) → write it to a cassette.
    pub fn take_captured(&self) -> HashMap<String, Recording> {
        match &self.mode {
            Mode::Record { captured, .. } => {
                std::mem::take(&mut *captured.lock().unwrap_or_else(|e| e.into_inner()))
            }
            Mode::Replay { .. } => HashMap::new(),
        }
    }
}

/// Serialize a captured map to a cassette file (sorted keys → stable,
/// diff-friendly). Best-effort: a write failure is logged, never panics.
fn write_cassette(map: &HashMap<String, Recording>, path: &std::path::Path) {
    if map.is_empty() {
        return;
    }
    let sorted: std::collections::BTreeMap<_, _> = map.iter().collect();
    match serde_json::to_string_pretty(&sorted) {
        Ok(json) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(path, json) {
                tracing::warn!(error = %e, path = %path.display(), "cassette flush failed");
            } else {
                tracing::debug!(path = %path.display(), entries = sorted.len(), "cassette flushed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "cassette serialize failed"),
    }
}

#[async_trait]
impl LlmProvider for CassetteProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let key = request_fingerprint(&req);
        match &self.mode {
            Mode::Replay { cassette } => match cassette.get(&key) {
                Some(events) => {
                    let out: Vec<Result<ChatEvent, ProviderError>> =
                        events.iter().cloned().map(Ok).collect();
                    Ok(Box::pin(stream::iter(out)))
                }
                None => Err(ProviderError::Internal(format!(
                    "cassette MISS for request fp={key} — an input the recording \
                     didn't cover (a prompt changed?). Re-record or fix the prompt."
                ))),
            },
            Mode::Record {
                inner,
                captured,
                flush_path,
            } => {
                // Collect the inner stream, capture the full event sequence,
                // then re-emit verbatim (collect-then-replay; recording is not
                // latency-sensitive). Only a clean stream (no transport error)
                // is cached — a failed call must not be frozen as a "response".
                let events: Vec<Result<ChatEvent, ProviderError>> =
                    inner.clone().stream(req, ctx).await?.collect().await;
                if events.iter().all(|e| e.is_ok()) {
                    let recording: Recording =
                        events.iter().map(|e| e.as_ref().unwrap().clone()).collect();
                    let snapshot = {
                        let mut guard = captured.lock().unwrap_or_else(|e| e.into_inner());
                        guard.insert(key, recording);
                        guard.clone()
                    };
                    // Flush after EVERY capture, not on Drop: a CLI host that
                    // exits via std::process::exit never runs destructors, so
                    // Drop-only flushing silently loses the whole recording.
                    if let Some(path) = flush_path {
                        write_cassette(&snapshot, path);
                    }
                }
                Ok(Box::pin(stream::iter(events)))
            }
        }
    }
}

impl Drop for CassetteProvider {
    fn drop(&mut self) {
        // Backstop only — the primary flush is per-capture in `stream`,
        // because a CLI host that exits via std::process::exit never runs
        // destructors. This catches the graceful-shutdown case.
        if let Mode::Record {
            captured,
            flush_path: Some(path),
            ..
        } = &self.mode
        {
            let map = captured.lock().unwrap_or_else(|e| e.into_inner());
            write_cassette(&map, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::mock::{CannedResponse, MockProvider};
    use tars_types::ModelHint;

    fn req(prompt: &str) -> ChatRequest {
        ChatRequest::user(ModelHint::Explicit("m".into()), prompt)
    }

    async fn collect_text(p: Arc<dyn LlmProvider>, r: ChatRequest) -> String {
        p.stream(r, RequestContext::test_default())
            .await
            .unwrap()
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

    async fn collect_tool_names(p: Arc<dyn LlmProvider>, r: ChatRequest) -> Vec<String> {
        p.stream(r, RequestContext::test_default())
            .await
            .unwrap()
            .filter_map(|e| async move {
                match e {
                    Ok(ChatEvent::ToolCallStart { name, .. }) => Some(name),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn record_then_replay_round_trips_by_fingerprint() {
        let real = MockProvider::with_responses(
            "real",
            vec![CannedResponse::text("FINDING_A"), CannedResponse::text("FINDING_B")],
        );
        let rec = CassetteProvider::record("cass", real);
        assert_eq!(collect_text(rec.clone(), req("file-1")).await, "FINDING_A");
        assert_eq!(collect_text(rec.clone(), req("file-2")).await, "FINDING_B");
        let cassette = rec.take_captured();
        assert_eq!(cassette.len(), 2);

        let play = CassetteProvider::replay("cass", cassette);
        assert_eq!(collect_text(play.clone(), req("file-1")).await, "FINDING_A");
        assert_eq!(collect_text(play.clone(), req("file-2")).await, "FINDING_B");
        // stable across repeats
        assert_eq!(collect_text(play.clone(), req("file-1")).await, "FINDING_A");
    }

    #[tokio::test]
    async fn replay_preserves_tool_calls_not_just_text() {
        // A white-box agent's response is a tool call, not text — the cassette
        // must replay it so a fixer tool loop can be frozen.
        use tars_types::{StopReason, Usage};
        let tool_resp = CannedResponse::Sequence(vec![
            ChatEvent::started("real"),
            ChatEvent::ToolCallStart { index: 0, id: "c1".into(), name: "fs.write_file".into() },
            ChatEvent::ToolCallEnd {
                index: 0,
                id: "c1".into(),
                parsed_args: serde_json::json!({"path": "a.rs", "content": "fixed"}),
                thought_signature: None,
            },
            ChatEvent::Finished { stop_reason: StopReason::ToolUse, usage: Usage::default() },
        ]);
        let real = MockProvider::with_responses("real", vec![tool_resp]);
        let rec = CassetteProvider::record("cass", real);
        assert_eq!(collect_tool_names(rec.clone(), req("fix")).await, vec!["fs.write_file"]);
        let cassette = rec.take_captured();

        let play = CassetteProvider::replay("cass", cassette);
        // the tool call survives record→replay
        assert_eq!(collect_tool_names(play.clone(), req("fix")).await, vec!["fs.write_file"]);
    }

    #[tokio::test]
    async fn replay_miss_is_a_signal() {
        let play = CassetteProvider::replay("cass", HashMap::new());
        let err = play
            .stream(req("uncovered"), RequestContext::test_default())
            .await;
        assert!(err.is_err(), "a cassette miss must surface as an error, not a silent wrong answer");
    }
}
