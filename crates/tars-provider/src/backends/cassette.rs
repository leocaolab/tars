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

/// Collapse run-varying paths so a recording replays across runs — AND across
/// machines/tempdirs. A white-box agent's system prompt embeds its absolute
/// worktree (`<root>/.arc/worktrees/fix-<id>`), where BOTH parts vary: the
/// `<id>` changes every run, and the absolute `<root>` prefix changes whenever
/// the repo lives at a different path (another checkout, a fresh tempdir, a CI
/// box). Left raw, the fingerprint differs and replay MISSes even though the
/// logical request is identical. So we collapse the WHOLE absolute path token
/// that ends in `worktrees/fix-<id>`, prefix included, to a single constant —
/// making the fingerprint path-portable, mirroring how the critic references
/// files by repo-RELATIVE path. This is fingerprint-only: the live prompt the
/// model sees still carries the real absolute cwd (its tools resolve against the
/// process cwd, not this hash). (Record and replay both run this, so they
/// agree.) Extend here as other volatile request substrings surface.
fn normalize_volatile(canon: &str) -> String {
    const NEEDLE: &str = "worktrees/fix-";
    const REPL: &str = "NORMROOT/worktrees/fix-NORM";
    let bytes = canon.as_bytes();
    let mut out = String::with_capacity(canon.len());
    let mut cursor = 0usize; // start of the not-yet-emitted region
    while let Some(rel) = canon[cursor..].find(NEEDLE) {
        let needle_start = cursor + rel;
        // Walk back to the start of the absolute path token so the run-varying
        // tmp/worktree PREFIX collapses too, not just the `fix-<id>` suffix. The
        // token runs until a JSON string delimiter (`"`, `\`) or whitespace.
        let mut path_start = needle_start;
        while path_start > cursor {
            let c = bytes[path_start - 1];
            if c == b'"' || c == b'\\' || (c as char).is_whitespace() {
                break;
            }
            path_start -= 1;
        }
        out.push_str(&canon[cursor..path_start]);
        // Skip the worktree id (an alnum/`-`/`_` run) after the needle.
        let after_needle = needle_start + NEEDLE.len();
        let id_len = canon[after_needle..]
            .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(canon.len() - after_needle);
        out.push_str(REPL);
        cursor = after_needle + id_len;
    }
    out.push_str(&canon[cursor..]);
    out
}

/// Stable fingerprint of a request's deterministic content. Record and replay
/// MUST compute it identically — both call this on the live `ChatRequest`, after
/// the same volatile-path normalization.
pub fn request_fingerprint(req: &ChatRequest, model: &str) -> String {
    // The request itself is model-agnostic content; the concrete model
    // is passed alongside (bound at service construction) and MUST
    // participate so recordings for different models don't collide.
    let body = serde_json::to_string(req).unwrap_or_else(|_| format!("{req:?}"));
    let canon = format!("model={model}\0{body}");
    let canon = normalize_volatile(&canon);
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

/// On-disk cassette: the recordings PLUS the recorded provider's capabilities.
/// Capabilities matter because arc builds a DIFFERENT request depending on
/// whether the provider advertises tool support (a fixer's request carries tool
/// defs); a replay that advertised a bare `text_only_baseline` produced a
/// tool-less request → a fingerprint MISS. Storing + replaying the recorded caps
/// keeps the request byte-identical. `recordings` is a `BTreeMap` for a stable,
/// diff-friendly file.
#[derive(serde::Serialize, serde::Deserialize)]
struct CassetteFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<Capabilities>,
    // No `#[serde(default)]` — a legacy bare-map cassette has no `recordings`
    // key, so it fails to parse as `CassetteFile` and falls back below.
    recordings: std::collections::BTreeMap<String, Recording>,
}

impl CassetteProvider {
    /// Replay from a loaded cassette (fingerprint → recorded event sequence),
    /// advertising a bare text-only baseline.
    pub fn replay(id: impl Into<ProviderId>, cassette: HashMap<String, Recording>) -> Arc<Self> {
        Self::replay_with_caps(id, cassette, None)
    }

    /// Replay advertising the RECORDED provider's capabilities (so arc rebuilds
    /// the identical request). `None` → text-only baseline (legacy cassettes).
    pub fn replay_with_caps(
        id: impl Into<ProviderId>,
        cassette: HashMap<String, Recording>,
        caps: Option<Capabilities>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            capabilities: caps
                .unwrap_or_else(|| Capabilities::text_only_baseline(Pricing::default())),
            mode: Mode::Replay { cassette },
        })
    }

    /// Record by wrapping a real provider; flush the captured map with `take`.
    pub fn record(id: impl Into<ProviderId>, inner: Arc<dyn LlmProvider>) -> Arc<Self> {
        Self::record_to(id, inner, None)
    }

    /// Record + flush to `flush_path` (if set) after every captured response.
    /// `seed` pre-loads already-recorded entries so a recording session split
    /// across multiple registry builds ACCUMULATES into the file instead of
    /// each build overwriting it with only its own captures.
    pub fn record_to(
        id: impl Into<ProviderId>,
        inner: Arc<dyn LlmProvider>,
        flush_path: Option<PathBuf>,
    ) -> Arc<Self> {
        Self::record_seeded(id, inner, flush_path, HashMap::new())
    }

    /// Like [`Self::record_to`] but pre-seeded with prior recordings.
    pub fn record_seeded(
        id: impl Into<ProviderId>,
        inner: Arc<dyn LlmProvider>,
        flush_path: Option<PathBuf>,
        seed: HashMap<String, Recording>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            capabilities: inner.capabilities().clone(),
            mode: Mode::Record {
                inner,
                captured: Mutex::new(seed),
                flush_path,
            },
        })
    }

    /// Load a cassette file for replay. New format carries `capabilities` +
    /// `recordings`; a legacy bare `{fingerprint: [events]}` map still loads
    /// (text-only baseline caps).
    pub fn replay_from_file(
        id: impl Into<ProviderId>,
        path: &std::path::Path,
    ) -> std::io::Result<Arc<Self>> {
        let raw = std::fs::read_to_string(path)?;
        if let Ok(file) = serde_json::from_str::<CassetteFile>(&raw) {
            let recordings: HashMap<String, Recording> = file.recordings.into_iter().collect();
            return Ok(Self::replay_with_caps(id, recordings, file.capabilities));
        }
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

/// Serialize a captured map + the recorded provider's capabilities to a cassette
/// file (sorted keys → stable, diff-friendly). Best-effort: a write failure is
/// logged, never panics.
fn write_cassette(map: &HashMap<String, Recording>, caps: &Capabilities, path: &std::path::Path) {
    if map.is_empty() {
        return;
    }
    let recordings: std::collections::BTreeMap<String, Recording> =
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let file = CassetteFile { capabilities: Some(caps.clone()), recordings };
    match serde_json::to_string_pretty(&file) {
        Ok(json) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(path, json) {
                tracing::warn!(error = %e, path = %path.display(), "cassette flush failed");
            } else {
                tracing::debug!(path = %path.display(), entries = file.recordings.len(), "cassette flushed");
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
        model: &str,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let key = request_fingerprint(&req, model);
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
                    inner.clone().stream(req, model, ctx).await?.collect().await;
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
                        write_cassette(&snapshot, &self.capabilities, path);
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
            write_cassette(&map, &self.capabilities, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::mock::{CannedResponse, MockProvider};

    fn req(prompt: &str) -> ChatRequest {
        ChatRequest::user(prompt)
    }

    async fn collect_text(p: Arc<dyn LlmProvider>, r: ChatRequest) -> String {
        p.stream(r, "test-model", RequestContext::test_default())
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
        p.stream(r, "test-model", RequestContext::test_default())
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
            .stream(req("uncovered"), "test-model", RequestContext::test_default())
            .await;
        assert!(err.is_err(), "a cassette miss must surface as an error, not a silent wrong answer");
    }

    #[test]
    fn fingerprint_ignores_the_run_varying_worktree_id() {
        // Two runs allocate different fixer worktrees; the prompt is otherwise
        // identical. They must hash the same so the recording replays.
        let run1 = r#"{"system":"Working directory (absolute): /tmp/r/.arc/worktrees/fix-1140ccbb\nfix it"}"#;
        let run2 = r#"{"system":"Working directory (absolute): /tmp/r/.arc/worktrees/fix-563e568e\nfix it"}"#;
        assert_ne!(run1, run2, "raw strings differ");
        assert_eq!(
            normalize_volatile(run1),
            normalize_volatile(run2),
            "the worktree id must be normalized out of the fingerprint",
        );
    }

    #[test]
    fn fingerprint_is_path_portable_across_worktree_roots() {
        // The fixer's system prompt grounds the model with its ABSOLUTE cwd. A
        // second checkout / a fresh tempdir / a CI box puts the repo at a
        // different absolute root, so the `worktrees/fix-<id>` PREFIX differs
        // even when the id would match. The logical request is identical, so the
        // fingerprint must collapse the whole path — not just the id — else the
        // FIRST fixer call MISSes when replayed from a different directory.
        let here = r#"{"system":"Working directory (absolute): /Users/dev/checkout-a/.arc/worktrees/fix-1140ccbb\nfix it"}"#;
        let there = r#"{"system":"Working directory (absolute): /private/tmp/.tmpXY9/repo/.arc/worktrees/fix-563e568e\nfix it"}"#;
        assert_ne!(here, there, "raw strings differ (different tmp/root prefixes)");
        assert_eq!(
            request_fingerprint_of(here),
            request_fingerprint_of(there),
            "a different worktree ROOT must not change the fingerprint",
        );

        // Guard the collapse isn't over-eager: a genuinely different prompt body
        // (same worktree path) must still fingerprint distinctly.
        let other_body = r#"{"system":"Working directory (absolute): /private/tmp/.tmpXY9/repo/.arc/worktrees/fix-563e568e\nDO SOMETHING ELSE"}"#;
        assert_ne!(
            request_fingerprint_of(there),
            request_fingerprint_of(other_body),
            "only the path is volatile; the rest of the prompt must still count",
        );

        // A prompt with no worktree path (the critic) passes through untouched.
        let critic = r#"{"system":"review crates/foo.rs against the rubric"}"#;
        assert_eq!(normalize_volatile(critic), critic, "non-worktree prompts are unchanged");
    }

    /// Hash a canonical string the same way `request_fingerprint` does, but
    /// straight from a `&str` so the test can exercise the volatile-path collapse
    /// without constructing a full `ChatRequest`.
    fn request_fingerprint_of(canon: &str) -> String {
        let canon = normalize_volatile(canon);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        canon.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    #[test]
    fn cassette_file_round_trips_capabilities() {
        // A recording stores the provider's caps; replay advertises them (so arc
        // rebuilds the identical, tool-carrying request). Legacy bare maps still load.
        let mut caps = Capabilities::text_only_baseline(Pricing::default());
        caps.supports_tool_use = true;
        let file = CassetteFile {
            capabilities: Some(caps.clone()),
            recordings: std::collections::BTreeMap::new(),
        };
        let json = serde_json::to_string(&file).unwrap();
        let back: CassetteFile = serde_json::from_str(&json).unwrap();
        assert!(back.capabilities.unwrap().supports_tool_use, "caps survive the cassette file");
        // legacy bare map fails to parse as CassetteFile (no `recordings` key)
        assert!(serde_json::from_str::<CassetteFile>(r#"{"abc":[]}"#).is_err());
    }
}
