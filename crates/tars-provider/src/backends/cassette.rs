//! Cassette provider — deterministic LLM replay for testing.
//!
//! Stays a provider: it serves recorded events rather than live ones, and
//! nothing more. Diffing a MISS, blessing a baseline, judging whether a change
//! is a regression is testing-layer work in the testing layer that consumes the miss. That
//! split is load-bearing: a MISS here reports the FACTS (the fingerprint, and
//! the recorded request when one was captured) and does not decide whether the
//! difference is acceptable — the same cassette pins "same result" for one test
//! and "same steps" for another, so only the consumer's assertions can judge.
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
    ProviderProfile, ChatEvent, ChatRequest, Pricing, ProviderError, ProviderId, RequestContext,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// One recorded call: the event sequence, plus the canonical request text the
/// fingerprint was taken over.
///
/// `request` is what makes a MISS diffable. It is optional because cassettes
/// recorded before this field exists must keep loading — for those, a MISS can
/// still only report the fingerprint, and the error says so instead of
/// pretending the diff is empty.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Recording {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// The consumer's label for this call (`cassette.step`), when it supplied
    /// one. It is what makes "the same step's previous recording" a
    /// deterministic lookup instead of a guess — see [`pick_baseline`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    pub events: Vec<ChatEvent>,
}

impl Recording {
    /// A recording with no captured request — the shape every pre-existing
    /// cassette on disk deserializes into.
    fn from_events(events: Vec<ChatEvent>) -> Self {
        Self { request: None, step: None, events }
    }
}

/// Accepts BOTH shapes: the legacy bare `[event, …]` array and the current
/// `{ request?, events }` object. Written by hand rather than `#[serde(untagged)]`
/// so a malformed object reports ITS error instead of the untagged
/// "data did not match any variant", which hides the real cause.
impl<'de> serde::Deserialize<'de> for Recording {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Obj {
            #[serde(default)]
            request: Option<String>,
            #[serde(default)]
            step: Option<String>,
            events: Vec<ChatEvent>,
        }
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Array(_) => serde_json::from_value::<Vec<ChatEvent>>(v)
                .map(Recording::from_events)
                .map_err(serde::de::Error::custom),
            _ => serde_json::from_value::<Obj>(v)
                .map(|o| Recording { request: o.request, step: o.step, events: o.events })
                .map_err(serde::de::Error::custom),
        }
    }
}

/// Pick the recording a MISS should be compared against, and say how it was
/// picked.
///
/// Selection only — no diffing and no rendering. Those are the testing layer's
/// job (the testing layer that consumes the miss); this side owns the recordings, so it is
/// the only place that CAN choose, and it hands the choice out as a fact.
///
/// Priority:
///   1. `label` — the consumer's `cassette.step`. Deterministic, and the only
///      option that survives concurrent calls.
///   2. `prefix` — longest shared prefix. A GUESS; callers must render it as
///      one, because a diff against the wrong baseline points at a change that
///      never happened.
///
/// (`seq` is absent on purpose: nothing here tracks position within a session,
/// and inventing one would be sound only while the journey stayed serial.)
///
/// A label narrows the candidates; it does not always single one out — a
/// multi-turn step (a tool loop) records one request PER TURN under the same
/// label. Within that narrowed set the shared prefix picks, and an exact tie
/// breaks on the fingerprint. Both stages must be total orders: taking the
/// first match out of a `HashMap` would make the baseline depend on iteration
/// order, so the same failure would diff against a different recording run to
/// run — nondeterminism dressed as a fact.
fn pick_baseline<'a>(
    want: &str,
    want_step: Option<&str>,
    cassette: &'a HashMap<String, Recording>,
) -> Option<(&'a String, &'a String, &'static str)> {
    let shared = |a: &str, b: &str| a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    // Ranked candidates among a subset, most-similar first, fingerprint-tiebroken.
    let best = |keep: &dyn Fn(&Recording) -> bool| {
        cassette
            .iter()
            .filter(|(_, r)| keep(r))
            .filter_map(|(fp, r)| r.request.as_ref().map(|q| (fp, q, shared(want, q))))
            .max_by(|(fa, _, na), (fb, _, nb)| na.cmp(nb).then_with(|| fb.cmp(fa).reverse()))
    };

    if let Some(step) = want_step.filter(|s| !s.is_empty()) {
        if let Some((fp, req, _)) = best(&|r: &Recording| r.step.as_deref() == Some(step)) {
            return Some((fp, req, "label"));
        }
    }
    let (fp, req, _) = best(&|_: &Recording| true)?;
    Some((fp, req, "prefix"))
}

/// The consumer's label for this call, read from the request context.
///
/// Lives in `attributes` because that map's stated purpose is passing values
/// through to inner layers — and "which step of the journey is this" is the
/// consumer's knowledge, not tars's: the orchestration is not in tars, so
/// nothing here could derive it.
pub const STEP_ATTR: &str = "cassette.step";

fn step_of(ctx: &RequestContext) -> Option<String> {
    ctx.read_attributes()
        .get(STEP_ATTR)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

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
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon_request(req, model).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The EXACT text the fingerprint is taken over, after volatile-path
/// normalization.
///
/// Recorded alongside every response so a MISS can be explained rather than
/// merely announced. A fingerprint alone tells you "the request changed" and
/// nothing else, which leaves re-recording as the only available move — and
/// re-recording an unexplained change stamps whatever drifted, regression
/// included, as the new baseline. Keeping the canonical text turns the golden
/// back into a witness: `nearest_diff` below shows WHICH bytes moved.
pub fn canon_request(req: &ChatRequest, model: &str) -> String {
    // The request itself is model-agnostic content; the concrete model
    // is passed alongside (bound at service construction) and MUST
    // participate so recordings for different models don't collide.
    let body = serde_json::to_string(req).unwrap_or_else(|_| format!("{req:?}"));
    normalize_volatile(&format!("model={model}\0{body}"))
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
    capabilities: ProviderProfile,
    mode: Mode,
}

/// On-disk cassette: the recordings PLUS the recorded provider's capabilities.
/// ProviderProfile matter because arc builds a DIFFERENT request depending on
/// whether the provider advertises tool support (a fixer's request carries tool
/// defs); a replay that advertised a bare `text_only_baseline` produced a
/// tool-less request → a fingerprint MISS. Storing + replaying the recorded caps
/// keeps the request byte-identical. `recordings` is a `BTreeMap` for a stable,
/// diff-friendly file.
#[derive(serde::Serialize, serde::Deserialize)]
struct CassetteFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<ProviderProfile>,
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
        caps: Option<ProviderProfile>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            capabilities: caps.unwrap_or_else(|| {
                // Legacy cassette with no recorded caps: text-only baseline
                // stamped Mock — cassette places no real call.
                let mut c = ProviderProfile::text_only_baseline(Pricing::default());
                c.interface = tars_types::InterfaceKind::Mock;
                c
            }),
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
fn write_cassette(map: &HashMap<String, Recording>, caps: &ProviderProfile, path: &std::path::Path) {
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
    fn capabilities(&self) -> &ProviderProfile {
        &self.capabilities
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Compute the canonical text once: it IS the fingerprint's input, and
        // both branches need it — replay to diff a miss, record to store it.
        let canon = canon_request(&req, model);
        let key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            canon.hash(&mut h);
            format!("{:016x}", h.finish())
        };
        debug_assert_eq!(key, request_fingerprint(&req, model), "canon and fingerprint must agree");
        match &self.mode {
            Mode::Replay { cassette } => match cassette.get(&key) {
                Some(rec) => {
                    let out: Vec<Result<ChatEvent, ProviderError>> =
                        rec.events.iter().cloned().map(Ok).collect();
                    Ok(Box::pin(stream::iter(out)))
                }
                None => {
                    let picked = pick_baseline(&canon, step_of(&ctx).as_deref(), cassette);
                    // Hand the located-miss facts to the testing layer as a
                    // structured error — the wanted fingerprint + canonical
                    // request, plus the nearest captured baseline (if any) and how
                    // it was chosen — so a located-diff renderer in the testing layer can
                    // render a located diff. The truth travels in the error, never
                    // a flattened sentinel string.
                    Err(ProviderError::CassetteMiss {
                        want_fp: key,
                        want_canon: canon,
                        baseline_fp: picked.map(|(fp, _, _)| fp.clone()),
                        baseline_canon: picked.map(|(_, q, _)| q.clone()),
                        baseline_selected_by: picked.map(|(_, _, by)| by.to_string()),
                    })
                }
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
                // Read the step label BEFORE the context is moved into the inner
                // provider: it identifies this call for a future miss, and a
                // recording made without it can only ever be found by guesswork.
                let step = step_of(&ctx);
                let events: Vec<Result<ChatEvent, ProviderError>> =
                    inner.clone().stream(req, model, ctx).await?.collect().await;
                if events.iter().all(|e| e.is_ok()) {
                    // Capture the canonical request beside the events: it is what
                    // a later MISS diffs against.
                    let recording = Recording {
                        request: Some(canon),
                        step,
                        events: events.iter().map(|e| e.as_ref().unwrap().clone()).collect(),
                    };
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
        let err = match play
            .stream(req("uncovered"), "test-model", RequestContext::test_default())
            .await
        {
            Ok(_) => panic!("a cassette miss must surface as an error, not a silent wrong answer"),
            Err(e) => e,
        };
        // The miss is the STRUCTURED error carrying the wanted fingerprint — not a
        // flattened Internal string. With an empty cassette there is no baseline.
        match err {
            ProviderError::CassetteMiss {
                want_fp,
                baseline_fp,
                baseline_selected_by,
                ..
            } => {
                assert!(!want_fp.is_empty(), "want_fp must carry the request fingerprint");
                assert!(baseline_fp.is_none(), "empty cassette → no baseline to diff");
                assert!(baseline_selected_by.is_none());
            }
            other => panic!("expected CassetteMiss, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_miss_carries_the_nearest_baseline() {
        // Record one call, then replay and ask for a DIFFERENT prompt: the miss
        // hands back the recorded call as the nearest baseline (prefix-picked),
        // with BOTH canonical requests, so a located-diff renderer in the testing layer
        // can locate where the request diverged. This is the producer side of the
        // structured miss — the facts must travel in the error, not a sentinel.
        let real = MockProvider::with_responses("real", vec![CannedResponse::text("A")]);
        let rec = CassetteProvider::record("cass", real);
        let _ = collect_text(rec.clone(), req("shared-prefix ONE")).await;
        let cassette = rec.take_captured();
        assert_eq!(cassette.len(), 1);

        let play = CassetteProvider::replay("cass", cassette);
        let err = match play
            .stream(req("shared-prefix TWO"), "test-model", RequestContext::test_default())
            .await
        {
            Ok(_) => panic!("a miss against a populated cassette is still an error"),
            Err(e) => e,
        };
        match err {
            ProviderError::CassetteMiss {
                want_fp,
                want_canon,
                baseline_fp,
                baseline_canon,
                baseline_selected_by,
            } => {
                assert!(!want_fp.is_empty());
                assert!(want_canon.contains("shared-prefix TWO"), "want_canon carries the request");
                assert!(baseline_fp.is_some(), "the recorded call is the nearest baseline");
                assert!(
                    baseline_canon.as_deref().unwrap().contains("shared-prefix ONE"),
                    "baseline_canon carries the recorded request to diff against"
                );
                assert_eq!(baseline_selected_by.as_deref(), Some("prefix"));
            }
            other => panic!("expected CassetteMiss, got {other:?}"),
        }
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
        let mut caps = ProviderProfile::text_only_baseline(Pricing::default());
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

    /// Cassettes recorded before request capture must keep loading — the field
    /// is additive, not a format break.
    #[test]
    fn a_recording_loads_from_the_legacy_bare_event_array() {
        let legacy: Recording = serde_json::from_str(r#"[{"type":"delta","text":"hi"}]"#)
            .expect("the pre-capture shape must still deserialize");
        assert_eq!(legacy.events.len(), 1);
        assert!(legacy.request.is_none(), "a legacy recording has no request to diff against");

        let current: Recording =
            serde_json::from_str(r#"{"request":"model=m body","events":[{"type":"delta","text":"hi"}]}"#)
                .expect("the current shape deserializes");
        assert_eq!(current.request.as_deref(), Some("model=m body"));
        assert_eq!(current.events.len(), 1);
    }

    /// A label makes "the same step's previous recording" a lookup, not a
    /// guess — and it must win even when another recording is textually closer.
    #[test]
    fn a_step_label_selects_the_baseline_over_the_closer_looking_one() {
        let mut cassette = HashMap::new();
        cassette.insert(
            "fp_other".to_string(),
            Recording {
                request: Some("model=m\nalmost exactly the live text".to_string()),
                step: Some("verify:F-1".to_string()),
                events: vec![],
            },
        );
        cassette.insert(
            "fp_same_step".to_string(),
            Recording {
                request: Some("model=m\ncompletely different bytes".to_string()),
                step: Some("review:lib.rs".to_string()),
                events: vec![],
            },
        );

        let (fp, _, by) = pick_baseline(
            "model=m\nalmost exactly the live text!",
            Some("review:lib.rs"),
            &cassette,
        )
        .expect("a labelled recording exists");
        assert_eq!(by, "label");
        assert_eq!(fp, "fp_same_step", "the same STEP wins over the closer TEXT");
    }

    /// With no label there is only the prefix heuristic — and it must report
    /// itself as such, because a diff against the wrong baseline points at a
    /// change that never happened.
    #[test]
    fn without_a_label_the_baseline_is_a_declared_guess() {
        let mut cassette = HashMap::new();
        cassette.insert(
            "fp_near".to_string(),
            Recording { request: Some("model=m shared head AAA".to_string()), step: None, events: vec![] },
        );
        cassette.insert(
            "fp_far".to_string(),
            Recording { request: Some("zzz unrelated".to_string()), step: None, events: vec![] },
        );

        let (fp, _, by) = pick_baseline("model=m shared head BBB", None, &cassette)
            .expect("a recorded request exists");
        assert_eq!(by, "prefix", "no label ⇒ the choice is a heuristic and says so");
        assert_eq!(fp, "fp_near");
    }

    /// Nothing captured ⇒ no baseline. Returning one anyway would let a caller
    /// render an empty diff, which reads as "nothing changed".
    #[test]
    fn no_captured_request_yields_no_baseline_rather_than_an_empty_one() {
        let mut cassette = HashMap::new();
        cassette.insert(
            "fp".to_string(),
            Recording { request: None, step: Some("review:lib.rs".into()), events: vec![] },
        );
        assert!(pick_baseline("anything", Some("review:lib.rs"), &cassette).is_none());
        assert!(pick_baseline("anything", None, &cassette).is_none());
    }

    /// A label NARROWS the candidates; a multi-turn step records one request per
    /// turn under the same label, so the choice within that set must still be a
    /// total order. Taking the first `HashMap` match would make the baseline
    /// depend on iteration order — the same failure would diff against a
    /// different turn run to run, which is nondeterminism reported as a fact.
    #[test]
    fn among_same_label_turns_the_closest_one_wins_deterministically() {
        let mut cassette = HashMap::new();
        for (fp, req) in [
            ("fp_turn1", "model=m\nTURN ONE entirely different"),
            ("fp_turn2", "model=m\nshared head, turn two"),
            ("fp_turn3", "model=m\nshared head, turn three"),
        ] {
            cassette.insert(
                fp.to_string(),
                Recording {
                    request: Some(req.to_string()),
                    step: Some("fix:F-1".into()),
                    events: vec![],
                },
            );
        }

        // Every run must agree — re-select repeatedly over a rehashed map.
        for _ in 0..20 {
            let shuffled: HashMap<String, Recording> = cassette.clone().into_iter().collect();
            let (fp, _, by) = pick_baseline("model=m\nshared head, turn thr", Some("fix:F-1"), &shuffled)
                .expect("labelled recordings exist");
            assert_eq!(by, "label");
            assert_eq!(fp, "fp_turn3", "the closest turn under the label, every time");
        }
    }
}
