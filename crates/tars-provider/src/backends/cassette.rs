//! Cassette provider — deterministic LLM replay for testing (Doc 18 §5a).
//!
//! Pins the LLM so a code-change A/B isolates the CODE, not model noise. Two
//! modes, ONE request-fingerprint function (so record and replay agree):
//!
//!   - **record**: wrap a real provider, pass its responses through, and capture
//!     `(request fingerprint → response text)` into a cassette file.
//!   - **replay**: serve the recorded response for a matching request; a **miss
//!     is a signal** (an input the recording didn't cover — usually a prompt that
//!     changed) and surfaces as a provider error, never a silent wrong answer.
//!
//! The fingerprint is a stable hash of the serialized `ChatRequest` (model +
//! system + messages + tools + schema) — `ChatRequest: Serialize` — so the same
//! logical request maps to the same key at record and replay time.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{stream, StreamExt};

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, Pricing, ProviderError, ProviderId, RequestContext,
    StopReason, Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};

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
    /// Pass through `inner`, capturing each (fingerprint → text) into `captured`.
    /// `flush_path` (if set) is written with the captured map when the provider
    /// drops at end of run — auto-VCR recording.
    Record {
        inner: Arc<dyn LlmProvider>,
        captured: Mutex<HashMap<String, String>>,
        flush_path: Option<PathBuf>,
    },
    /// Serve recorded text by fingerprint; a miss is an error (signal).
    Replay { cassette: HashMap<String, String> },
}

pub struct CassetteProvider {
    id: ProviderId,
    capabilities: Capabilities,
    mode: Mode,
}

impl CassetteProvider {
    /// Replay from a loaded cassette (fingerprint → response text).
    pub fn replay(id: impl Into<ProviderId>, cassette: HashMap<String, String>) -> Arc<Self> {
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

    /// Record + auto-flush to `flush_path` (if set) when the provider drops.
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

    /// Load a cassette file (`{fingerprint: response_text}` JSON) for replay.
    pub fn replay_from_file(
        id: impl Into<ProviderId>,
        path: &std::path::Path,
    ) -> std::io::Result<Arc<Self>> {
        let raw = std::fs::read_to_string(path)?;
        let cassette: HashMap<String, String> = serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self::replay(id, cassette))
    }

    /// Drain everything captured so far (record mode) → write it to a cassette.
    pub fn take_captured(&self) -> HashMap<String, String> {
        match &self.mode {
            Mode::Record { captured, .. } => std::mem::take(
                &mut *captured.lock().unwrap_or_else(|e| e.into_inner()),
            ),
            Mode::Replay { .. } => HashMap::new(),
        }
    }
}

impl Drop for CassetteProvider {
    fn drop(&mut self) {
        // Auto-VCR: on run end, a recording provider writes its captured
        // (fingerprint → response) map to disk. Sorted keys for a stable,
        // diff-friendly cassette. Best-effort — a write failure here must
        // not panic a teardown, so it's logged, not propagated.
        if let Mode::Record {
            captured,
            flush_path: Some(path),
            ..
        } = &self.mode
        {
            let map = std::mem::take(&mut *captured.lock().unwrap_or_else(|e| e.into_inner()));
            if map.is_empty() {
                return;
            }
            let sorted: std::collections::BTreeMap<_, _> = map.into_iter().collect();
            match serde_json::to_string_pretty(&sorted) {
                Ok(json) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(e) = std::fs::write(path, json) {
                        tracing::warn!(error = %e, path = %path.display(), "cassette flush failed");
                    } else {
                        tracing::info!(path = %path.display(), entries = sorted.len(), "cassette recorded");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "cassette serialize failed"),
            }
        }
    }
}

fn text_events(text: String, model: String) -> Vec<Result<ChatEvent, ProviderError>> {
    vec![
        Ok(ChatEvent::started(model)),
        Ok(ChatEvent::Delta { text: text.clone() }),
        Ok(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                output_tokens: text.len() as u64 / 4,
                ..Default::default()
            },
        }),
    ]
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
        let model = req.model.label().to_string();
        match &self.mode {
            Mode::Replay { cassette } => match cassette.get(&key) {
                Some(text) => Ok(Box::pin(stream::iter(text_events(text.clone(), model)))),
                None => Err(ProviderError::Internal(format!(
                    "cassette MISS for request fp={key} — an input the recording \
                     didn't cover (a prompt changed?). Re-record or fix the prompt."
                ))),
            },
            Mode::Record {
                inner, captured, ..
            } => {
                // Collect the inner stream, aggregate the text, capture it,
                // then re-emit verbatim (collect-then-replay; recording is
                // not latency-sensitive).
                let events: Vec<Result<ChatEvent, ProviderError>> =
                    inner.clone().stream(req, ctx).await?.collect().await;
                let text: String = events
                    .iter()
                    .filter_map(|e| match e {
                        Ok(ChatEvent::Delta { text }) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                captured
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, text);
                Ok(Box::pin(stream::iter(events)))
            }
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

    #[tokio::test]
    async fn record_then_replay_round_trips_by_fingerprint() {
        let real = MockProvider::with_responses(
            "real",
            vec![CannedResponse::text("FINDING_A"), CannedResponse::text("FINDING_B")],
        );
        let rec = CassetteProvider::record("cass", real);
        // record two distinct requests
        assert_eq!(collect_text(rec.clone(), req("file-1")).await, "FINDING_A");
        assert_eq!(collect_text(rec.clone(), req("file-2")).await, "FINDING_B");
        let cassette = rec.take_captured();
        assert_eq!(cassette.len(), 2);

        // replay returns the SAME response per request, deterministically
        let play = CassetteProvider::replay("cass", cassette);
        assert_eq!(collect_text(play.clone(), req("file-1")).await, "FINDING_A");
        assert_eq!(collect_text(play.clone(), req("file-2")).await, "FINDING_B");
        // and again — stable
        assert_eq!(collect_text(play.clone(), req("file-1")).await, "FINDING_A");
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
