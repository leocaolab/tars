//! Claude SDK child-process backend.
//!
//! Why it exists: the `claude_cli` backend spawns a fresh `claude -p`
//! subprocess per call. Each spawn pays a Node-runtime cold start +
//! OAuth handshake + ~128k-token internal system-prompt cache load
//! (the CLI silently runs `num_turns=5` worth of internal routing).
//! Empirically this costs 60-300s per call on realistic prompts and
//! trips tars's circuit breaker after a few timeouts.
//!
//! How this backend fixes it: spawn one long-lived Node child running
//! `tools/claude-daemon/server.mjs --stdio`. The child embeds
//! `@anthropic-ai/claude-agent-sdk`, holds OAuth + the prompt cache
//! warm for the rest of tars's lifetime, and serves N concurrent
//! requests over its single stdin (NDJSON requests) / stdout (NDJSON
//! replies) pipe pair. Each request carries an `id`; a background
//! reader task demuxes the replies back to per-call `oneshot`
//! channels.
//!
//! Lifecycle: child is spawned **lazily** on the first `stream()` call
//! (so just constructing a provider doesn't pay the warm-up cost
//! upfront), reused for all subsequent calls, and killed on `Drop`
//! (`tokio::process::Command::kill_on_drop(true)`). If the child
//! exits — clean or crashed — the reader task drains pending requests
//! with [`ProviderError::Internal`] and the next `stream()` call
//! transparently respawns.
//!
//! Wire shape (per line on each pipe):
//! ```json
//! stdin:  { "id": u64, "prompt": str, "system"?: str,
//!           "model"?: str, "max_turns"?: int }
//! stdout: { "id": u64,
//!           // either reply fields…
//!           "text"?: str, "usage"?: {...}, "model"?: str,
//!           "result_subtype"?: str, "durations"?: {...},
//!           "message_count"?: int,
//!           // …or an error envelope
//!           "error"?: str, "stack"?: str }
//! ```

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, ContentBlock, Message, Modality, PromptCacheKind,
    ProviderError, ProviderId, RequestContext, StopReason, StructuredOutputMode, Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// Per-call timeout to the daemon. History: defaulted at 300s (5 min)
/// which was fine for per-file L4 critic calls (~10-60s typical).
///
/// Bumped to 900s (15 min) after the first end-to-end `arc auto` run
/// on tars (154 files, 264 findings): the Critic re-review pass after
/// Round 1 fix is a single batched call that examines every applied
/// fix at once, and it tripped the 300s ceiling. Without the verify
/// pass, arc commits Round 1 fixes blind (the [Critic] Verifying
/// fixes... step is what catches Agent regressions before they land).
///
/// 900s is the headroom verify needs at the 200-300 finding scale.
/// Per-file critic and per-file fix calls still finish well under 300s
/// in practice, so the bump is a ceiling, not a typical wait.
const DEFAULT_TIMEOUT_SECS: u64 = 900;

/// In-flight requests keyed by their wire `id`. The reader task removes
/// the entry on the matching reply line; the `call()` path removes it
/// if it hits the per-call timeout first (so a late reply doesn't try
/// to send into a dropped sender).
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<DaemonChatReply, ProviderError>>>>>;

pub struct ClaudeSdkProviderBuilder {
    id: ProviderId,
    executable: String,
    script_path: Option<String>,
    default_model: Option<String>,
    timeout: Duration,
    capabilities: Option<Capabilities>,
}

impl ClaudeSdkProviderBuilder {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            executable: "node".into(),
            script_path: None,
            default_model: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            capabilities: None,
        }
    }

    builder_setter!(executable: into String);
    builder_setter!(script_path: into_opt String);
    builder_setter!(default_model: into_opt String);
    builder_setter!(timeout: Duration);
    builder_setter!(capabilities: opt Capabilities);

    pub fn build(self) -> Arc<ClaudeSdkProvider> {
        // Builders for the other CLI-shaped backends (ClaudeCli, GeminiCli)
        // are infallible and accept a missing script_path / executable
        // by letting validation surface the error at config time. Mirror
        // that — if script_path is `None` here, fail loudly on the first
        // call rather than at construction.
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        Arc::new(ClaudeSdkProvider {
            id: self.id,
            executable: self.executable,
            script_path: self.script_path,
            default_model: self.default_model,
            timeout: self.timeout,
            capabilities: caps,
            session: Mutex::new(None),
        })
    }
}

fn default_capabilities() -> Capabilities {
    let mut modalities = std::collections::HashSet::new();
    modalities.insert(Modality::Text);
    Capabilities {
        max_context_tokens: 200_000,
        max_output_tokens: 8_192,
        // Child strips tools (server.mjs sets disallowedTools: ['*']);
        // this provider is intentionally pure-LLM.
        supports_tool_use: false,
        supports_parallel_tool_calls: false,
        // The daemon wires the request's JSON Schema to the Agent SDK's
        // `outputFormat: {type:'json_schema'}` — separate from agentic
        // tools, so strict schema works even with tools fully disabled.
        supports_structured_output: StructuredOutputMode::StrictSchema,
        supports_vision: false,
        supports_thinking: false,
        supports_cancel: true,
        // The SDK uses Anthropic prompt caching transparently — same
        // shape as `claude_cli`.
        prompt_cache: PromptCacheKind::Delegated,
        // We aggregate the SDK's events inside the daemon and reply
        // once. Treat as non-streaming from tars's POV.
        streaming: false,
        modalities_in: modalities.clone(),
        modalities_out: std::collections::HashSet::from([Modality::Text]),
        pricing: tars_types::Pricing::default(),
    }
}

pub struct ClaudeSdkProvider {
    id: ProviderId,
    executable: String,
    script_path: Option<String>,
    default_model: Option<String>,
    timeout: Duration,
    capabilities: Capabilities,
    /// Lazily-initialized child session. `None` until the first
    /// `stream()` call (or after a child crash that the reader task
    /// noticed and cleared).
    session: Mutex<Option<Arc<Session>>>,
}

#[async_trait]
impl LlmProvider for ClaudeSdkProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    // `#[instrument(err(Display))]` is the Pythonic "uncaught exception
    // prints to stderr" boundary: when this fn returns Err, tracing
    // automatically emits an error-level event with the error's
    // Display form *plus* the function's span context (provider id,
    // model). Zero per-Err-site code, one log line per failed call.
    // Without it, an `Err(ProviderError::Internal(...))` walks the
    // stack silently — the operator only learns "something failed" if
    // some outer caller happens to log it, which today they don't.
    #[tracing::instrument(
        name = "claude_sdk.stream",
        skip_all,
        fields(
            provider = %self.id,
            model = %req.model.label(),
        ),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let resp = self.call(&req).await?;
        // Daemon returns a single JSON; project as canonical streaming
        // triple so callers' aggregation paths work unchanged.
        let usage = normalize_usage(&resp.usage);
        let actual_model = resp
            .model
            .unwrap_or_else(|| self.default_model.clone().unwrap_or_default());
        let stop_reason = map_stop_reason(resp.result_subtype.as_deref());
        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started(actual_model)),
            Ok(ChatEvent::Delta { text: resp.text }),
            Ok(ChatEvent::Finished { stop_reason, usage }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

impl ClaudeSdkProvider {
    /// Issue one chat request. Acquires (or spawns) the session,
    /// registers a pending oneshot, writes the request line, awaits
    /// the matching reply with the configured timeout.
    async fn call(&self, req: &ChatRequest) -> Result<DaemonChatReply, ProviderError> {
        let prompt = serialize_messages(req);
        if prompt.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "claude_sdk: request has no user-visible content".into(),
            ));
        }
        let model = req
            .model
            .explicit()
            .map(str::to_owned)
            .or_else(|| self.default_model.clone());

        let session = self.ensure_session().await?;
        // Monotonic per-session request id. `fetch_add` wraps on
        // overflow, but at u64 that's 2^64 requests on a single warm
        // child — at even a million calls/sec it would take ~580,000
        // years to wrap, and the session is respawned (resetting the
        // counter) long before that on any crash/restart. A collision is
        // therefore unreachable in practice; u64 is already the widest
        // sensible counter, so we document rather than widen.
        let id = session.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Result<DaemonChatReply, ProviderError>>();
        session.pending.lock().await.insert(id, tx);

        let body = ChatLine {
            id,
            prompt: &prompt,
            system: req.system.as_deref(),
            model: model.as_deref(),
            // History: 1 → 3 → 7. The 1→3 bump fit sonnet-4-5
            // extended thinking (`thinking_block → text_block` is
            // counted as 2 turns). 3→7 covers heavier
            // think-iterate-refine patterns the L4 critic exhibits
            // on dense files: under `arc auto` we saw "Reached
            // maximum number of turns (3)" on ~3% of 154 files
            // (validation/builtin.rs, etc.) where the model wants
            // to think → draft → re-read → refine before emitting
            // its verdict. 7 covers up to 3 think/refine rounds
            // plus the final answer; tools are still disabled on
            // the daemon side so the model can't go agentic
            // regardless of how high this counter climbs.
            max_turns: 7,
            schema: req.structured_output.as_ref().map(|s| &s.schema),
        };
        let mut line = serde_json::to_string(&body)
            .map_err(|e| ProviderError::Internal(format!("claude_sdk: encode body: {e}")))?;
        line.push('\n');

        // The timeout MUST cover the stdin WRITE, not just the reply wait.
        // The daemon shares ONE stdin pipe across N concurrent requests; when
        // it's busy emitting a dense file's long structured-output reply it
        // stops draining stdin, the pipe buffer fills, and `write_all` blocks
        // — and the OLD code only guarded `rx`, so that stuck write hung
        // forever and the per-call timeout never fired (observed: a claude_sdk
        // reviewer wedged >40 min on llm_client.rs / validators.rs, far past
        // the 900 s timeout). Guard the whole request→reply round-trip.
        let outcome = tokio::time::timeout(self.timeout, async {
            {
                let mut stdin = session.stdin.lock().await;
                stdin.write_all(line.as_bytes()).await.map_err(|e| {
                    ProviderError::Internal(format!(
                        "claude_sdk: write to child stdin failed: {e}"
                    ))
                })?;
                stdin.flush().await.map_err(|e| {
                    ProviderError::Internal(format!(
                        "claude_sdk: flush child stdin failed: {e}"
                    ))
                })?;
            }
            // `rx` resolves to the daemon's `Result<reply, ProviderError>`;
            // a RecvError means the reader task dropped the sender.
            match rx.await {
                Ok(result) => result,
                Err(_) => Err(ProviderError::Internal(
                    "claude_sdk: pending channel dropped (reader task gone)".into(),
                )),
            }
        })
        .await;

        match outcome {
            Ok(Ok(reply)) => Ok(reply),
            // A write/flush error or a reader-gone error inside the round-trip:
            // drop our pending slot and evict the (probably dead) session so
            // the NEXT call respawns instead of reusing a broken pipe.
            Ok(Err(e)) => {
                session.pending.lock().await.remove(&id);
                self.clear_session(&session).await;
                Err(e)
            }
            // Timed out ANYWHERE in the round-trip — including a stuck stdin
            // write. Reclaim the pending slot AND evict the wedged session
            // (the old code only removed pending; a daemon stuck not-draining
            // stdin would wedge every subsequent call too).
            Err(_) => {
                session.pending.lock().await.remove(&id);
                self.clear_session(&session).await;
                Err(ProviderError::Internal(format!(
                    "claude_sdk: timed out in request/reply round-trip after {:?}",
                    self.timeout
                )))
            }
        }
    }

    /// Hand out the warm session, or spawn one if there isn't a live
    /// child yet. Held under a single async-Mutex so a 4-concurrent
    /// `arc review` racing to the first call can't double-spawn.
    async fn ensure_session(&self) -> Result<Arc<Session>, ProviderError> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let s = Arc::new(self.spawn_session().await?);
        *guard = Some(s.clone());
        Ok(s)
    }

    /// Evict `dead` from the session cache so the next `ensure_session`
    /// respawns. Only clears if the cached session is *still* the one
    /// that died — under concurrency another caller may have already
    /// noticed the crash and respawned a fresh child, and we must not
    /// throw that healthy session away.
    async fn clear_session(&self, dead: &Arc<Session>) {
        let mut guard = self.session.lock().await;
        if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, dead)) {
            *guard = None;
        }
    }

    async fn spawn_session(&self) -> Result<Session, ProviderError> {
        // If the config pinned a path, honor it verbatim — the user knows
        // where their daemon lives. Otherwise walk the standard search
        // chain (env var → CWD ancestors → ~/.tars/). Owned `PathBuf`
        // because the searched candidates outlive `Self`.
        let resolved = match self.script_path.as_deref() {
            Some(p) => std::path::PathBuf::from(p),
            None => find_default_script_path().ok_or_else(|| {
                ProviderError::Internal(
                    "claude_sdk: no `script_path` configured and none of the default \
                     locations exist (checked $TARS_CLAUDE_SDK_SCRIPT, \
                     `tools/claude-daemon/server.mjs` walking up from CWD, \
                     and `~/.tars/claude-daemon/server.mjs`)"
                        .into(),
                )
            })?,
        };
        let script: &str = resolved.to_str().ok_or_else(|| {
            ProviderError::Internal(format!(
                "claude_sdk: script_path {resolved:?} is not valid UTF-8"
            ))
        })?;

        let mut cmd = Command::new(&self.executable);
        cmd.args([script, "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Pass child stderr through so `claude-daemon stdio ready`
            // and any SDK warnings land in the operator's terminal —
            // critical for debugging OAuth / SDK init failures.
            .stderr(Stdio::inherit())
            // Reap when this provider is dropped; without this, the
            // child outlives tars on `panic!` / `process::exit`.
            .kill_on_drop(true);
        // Put the daemon in its OWN process group so the signal-time reaper
        // can SIGKILL the daemon AND the claude children it spawns as a unit
        // (negative-PID group kill). kill_on_drop only covers the graceful
        // Drop path; a SIGINT/SIGTERM to tars never unwinds, so without this
        // the daemon + its claude children orphan.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            ProviderError::Internal(format!(
                "claude_sdk: spawn {:?} {:?}: {e}",
                self.executable, script
            ))
        })?;

        // Register the daemon PID so the host's SIGINT/SIGTERM handler can
        // reap it (and its process group) on signal-death. The graceful
        // path is handled by the `ReaperGuard` stored on the `Session`,
        // which deregisters on `Session` drop alongside kill_on_drop.
        let reaper_guard = child.id().map(crate::child_reaper::ReaperGuard::new);

        let stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Internal("claude_sdk: child stdin pipe missing".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::Internal("claude_sdk: child stdout pipe missing".into())
        })?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // Reader task: drain stdout line-by-line, demux each line to
        // the matching pending oneshot by `id`. On EOF, drain the map
        // with an error so callers in flight don't hang forever.
        let pending_for_reader = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Err(e) = dispatch_reply_line(&line, pending_for_reader.clone()).await
                        {
                            tracing::warn!(
                                error = %e,
                                line = %line,
                                "claude_sdk reader: dropping malformed reply line",
                            );
                        }
                    }
                    Ok(None) => {
                        // Clean EOF — child exited.
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "claude_sdk reader: stdout read error");
                        break;
                    }
                }
            }
            // Drain any pending requests with a clear error so callers
            // unblock instead of hanging on the oneshot.
            let mut p = pending_for_reader.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(ProviderError::Internal(
                    "claude_sdk: child exited before responding".into(),
                )));
            }
        });

        // Keep the `Child` alive on the session so the configured
        // `kill_on_drop(true)` actually fires when the session is
        // dropped. (Dropping `Child` is what triggers kill.)
        Ok(Session {
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(0),
            _child: Mutex::new(Some(child)),
            _reaper_guard: reaper_guard,
        })
    }
}

/// One in-flight reply being routed. Pulled out of `spawn_session` so
/// the reader loop stays readable.
async fn dispatch_reply_line(line: &str, pending: PendingMap) -> Result<(), ProviderError> {
    let raw: ReplyLine = serde_json::from_str(line)
        .map_err(|e| ProviderError::Parse(format!("decode reply: {e}")))?;
    let id = raw.id;
    // A well-formed reply carries exactly one of `error` / `text`. A
    // reply with neither (both null/absent) is a malformed daemon
    // envelope — treating it as success-with-empty-text would mask a
    // protocol bug as a blank completion, so surface it as a parse error.
    let result: Result<DaemonChatReply, ProviderError> = match (raw.error, raw.text) {
        (Some(err), _) => Err(ProviderError::Internal(format!("claude_sdk daemon: {err}"))),
        (None, Some(text)) => Ok(DaemonChatReply {
            text,
            result_subtype: raw.result_subtype,
            usage: raw.usage,
            model: raw.model,
        }),
        (None, None) => Err(ProviderError::Parse(format!(
            "claude_sdk daemon: reply id={id} has neither `error` nor `text`"
        ))),
    };
    if let Some(tx) = pending.lock().await.remove(&id) {
        let _ = tx.send(result);
    } else {
        tracing::warn!(
            id = id,
            "claude_sdk reader: reply with unknown id (caller already gave up?)",
        );
    }
    Ok(())
}

struct Session {
    stdin: Mutex<ChildStdin>,
    pending: PendingMap,
    next_id: AtomicU64,
    /// Holds the `Child` handle just so its `Drop` (and the
    /// `kill_on_drop(true)` flag we set at spawn) actually fire when
    /// the session is dropped. Never read otherwise — the mutex is
    /// only there to satisfy `Sync` without a Cell wrapper.
    _child: Mutex<Option<tokio::process::Child>>,
    /// Deregisters the daemon PID from the signal-time reaper registry on
    /// `Session` drop (the graceful path), parallel to `_child`'s
    /// kill_on_drop. `None` only if the OS didn't hand back a PID. Never
    /// read — held purely for its `Drop`.
    _reaper_guard: Option<crate::child_reaper::ReaperGuard>,
}

/// Resolve a default `server.mjs` location when the user omits
/// `script_path` from config. Returns `Some(path)` for the first
/// existing candidate, `None` if none of them resolve.
///
/// Search order (first hit wins):
/// 1. `$TARS_CLAUDE_SDK_SCRIPT` — explicit override for unusual layouts.
/// 2. `tools/claude-daemon/server.mjs` walking up from CWD — catches
///    "running inside the tars checkout" and "the reviewed repo
///    happens to vendor a copy". Stops at the filesystem root.
/// 3. `$HOME/.tars/claude-daemon/server.mjs` — standard per-user
///    install. (`tars install-claude-daemon` lays it down here.)
fn find_default_script_path() -> Option<std::path::PathBuf> {
    // Both env reads go through tars-types::env (ARC-L5-COH-18). The
    // `current_dir()` lookup stays inline — it's not a config knob,
    // it's a workspace-discovery probe (where am I running from), so
    // hoisting it would put a non-config lookup in the config module.
    if let Some(pb) = tars_types::env::claude_sdk_script_override()
        && pb.exists()
    {
        return Some(pb);
    }
    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors() {
            let candidate = ancestor.join("tools/claude-daemon/server.mjs");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    if let Some(home) = tars_types::env::home_dir() {
        let candidate = home.join(".tars/claude-daemon/server.mjs");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Flatten the ChatRequest into the single `prompt` string the daemon
/// expects. Multi-turn dialogues are tagged with `[role]` headers —
/// same shape as `claude_cli.rs::serialize_messages_for_cli`, so a
/// finding generated under one backend reads the same way under the
/// other.
fn serialize_messages(req: &ChatRequest) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        let (role, content) = match m {
            Message::User { content } => ("user", content),
            Message::Assistant { content, .. } => ("assistant", content),
            Message::Tool { content, .. } => ("tool", content),
            Message::System { content } => ("system", content),
        };
        let flat = flatten_blocks(content);
        if !flat.is_empty() {
            parts.push(format!("[{role}]\n{flat}"));
        }
    }
    parts.join("\n\n")
}

fn flatten_blocks(blocks: &[ContentBlock]) -> String {
    let mut out: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } => out.push(text.clone()),
            ContentBlock::Image { mime, .. } => out.push(format!("[image:{mime}]")),
        }
    }
    out.join("\n")
}

/// Anthropic-shape (disjoint) → tars-shape (OpenAI-shape: input_tokens
/// is the *total* prompt and includes cached + cache_creation). Same
/// normalization the [`crate::backends::anthropic`] adapter does.
fn normalize_usage(raw: &Option<RawUsage>) -> Usage {
    let Some(u) = raw else {
        return Usage::default();
    };
    let cached = u.cache_read_input_tokens.unwrap_or(0);
    let created = u.cache_creation_input_tokens.unwrap_or(0);
    let fresh = u.input_tokens.unwrap_or(0);
    Usage {
        input_tokens: fresh + cached + created,
        output_tokens: u.output_tokens.unwrap_or(0),
        cached_input_tokens: cached,
        cache_creation_tokens: created,
        ..Default::default()
    }
}

fn map_stop_reason(subtype: Option<&str>) -> StopReason {
    match subtype {
        Some("success") | Some("end_turn") | None => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("error_during_execution") | Some("error_max_turns") => StopReason::Other,
        Some(_) => StopReason::Other,
    }
}

// ─── Wire types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatLine<'a> {
    id: u64,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    max_turns: u32,
    /// JSON Schema for provider-level structured output. The daemon maps
    /// it to the Agent SDK's `outputFormat: {type:'json_schema', schema}`
    /// and returns the constrained answer as the reply text.
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<&'a serde_json::Value>,
}

/// Reply lines carry the `id` alongside either reply fields or an
/// `error` envelope. We accept both shapes in one struct so the
/// dispatcher doesn't have to peek before parsing — a `None` text +
/// `Some` error means failure, otherwise success.
#[derive(Debug, Deserialize)]
struct ReplyLine {
    id: u64,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    result_subtype: Option<String>,
    #[serde(default)]
    usage: Option<RawUsage>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug)]
struct DaemonChatReply {
    text: String,
    result_subtype: Option<String>,
    usage: Option<RawUsage>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::ModelHint;

    #[test]
    fn serialize_single_user_message() {
        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "ping");
        assert_eq!(serialize_messages(&req), "[user]\nping");
    }

    #[test]
    fn normalize_usage_sums_into_input_total() {
        let raw = Some(RawUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            cache_read_input_tokens: Some(900),
            cache_creation_input_tokens: Some(10),
        });
        let u = normalize_usage(&raw);
        assert_eq!(u.input_tokens, 1010); // 100 + 900 + 10
        assert_eq!(u.cached_input_tokens, 900);
        assert_eq!(u.cache_creation_tokens, 10);
        assert_eq!(u.output_tokens, 50);
    }

    #[test]
    fn normalize_usage_missing_returns_default() {
        // Usage doesn't implement PartialEq — check field-by-field.
        let u = normalize_usage(&None);
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cached_input_tokens, 0);
        assert_eq!(u.cache_creation_tokens, 0);
    }

    #[test]
    fn map_stop_reason_known_subtypes() {
        assert_eq!(map_stop_reason(Some("success")), StopReason::EndTurn);
        assert_eq!(map_stop_reason(None), StopReason::EndTurn);
        assert_eq!(map_stop_reason(Some("max_tokens")), StopReason::MaxTokens);
        assert_eq!(map_stop_reason(Some("nonsense")), StopReason::Other);
    }

    /// End-to-end smoke: spawn a real `node server.mjs --stdio` child
    /// and round-trip a request. `#[ignore]` so CI / plain `cargo test`
    /// skip it unless the operator has a working node + the SDK
    /// installed. Run with:
    ///   cargo test -p tars-provider --lib backends::claude_sdk -- --ignored
    #[tokio::test]
    #[ignore = "spawns node + uses live Anthropic OAuth"]
    async fn smoke_stdio_roundtrip() {
        // No `script_path` → exercises the default search chain
        // (CWD-ancestor lookup should find `tools/claude-daemon/server.mjs`
        // in the worktree). If the operator's CWD isn't inside the
        // worktree, set `TARS_CLAUDE_SDK_SCRIPT` to override.
        let p = ClaudeSdkProviderBuilder::new("claude_sdk_smoke")
            .default_model("claude-sonnet-4-5")
            .build();
        let req = ChatRequest::user(
            ModelHint::Explicit("claude-sonnet-4-5".into()),
            "Reply with only the literal word: pong",
        );
        let resp = p
            .clone()
            .complete(req, tars_types::RequestContext::test_default())
            .await
            .expect("daemon round-trip");
        let text = resp.text.to_lowercase();
        assert!(
            text.contains("pong"),
            "expected reply to contain 'pong', got: {:?}",
            resp.text
        );
        assert!(resp.usage.output_tokens >= 1);
    }

    /// Two concurrent requests on the same provider — proves the
    /// reader task demuxes correctly by `id`. The two replies can land
    /// in either order.
    #[tokio::test]
    #[ignore = "spawns node + uses live Anthropic OAuth"]
    async fn smoke_stdio_concurrent_requests() {
        // Same default-search path as `smoke_stdio_roundtrip`.
        let p = ClaudeSdkProviderBuilder::new("claude_sdk_smoke_concurrent")
            .default_model("claude-sonnet-4-5")
            .build();
        let req_a = ChatRequest::user(
            ModelHint::Explicit("claude-sonnet-4-5".into()),
            "Reply with only: alpha",
        );
        let req_b = ChatRequest::user(
            ModelHint::Explicit("claude-sonnet-4-5".into()),
            "Reply with only: beta",
        );
        let p2 = p.clone();
        let p3 = p.clone();
        let (a, b) = tokio::join!(
            p2.complete(req_a, tars_types::RequestContext::test_default()),
            p3.complete(req_b, tars_types::RequestContext::test_default()),
        );
        let a = a.expect("alpha");
        let b = b.expect("beta");
        assert!(a.text.to_lowercase().contains("alpha"), "{:?}", a.text);
        assert!(b.text.to_lowercase().contains("beta"), "{:?}", b.text);
    }

    /// Regression: the per-call timeout MUST cover the stdin WRITE, not just
    /// the reply wait. A daemon wedged in a rate-limit backoff (the real
    /// claude_sdk reviewer-hang root cause: an `rate_limit_event` makes the
    /// Agent SDK sleep, so the single-threaded daemon stops draining stdin)
    /// fills the stdin pipe buffer; `write_all` then blocks. The OLD code
    /// guarded only `rx`, so that blocked write hung FOREVER and the 900 s
    /// per-call timeout never fired (observed: a reviewer wedged >40 min).
    /// Here a fake daemon prints the ready line then spins without ever
    /// reading stdin; with a >64 KB prompt the write blocks, and the call
    /// must time out (Err) near the budget instead of hanging.
    #[tokio::test]
    #[ignore = "spawns a node child; verifies timeout covers a stuck stdin write"]
    async fn call_times_out_when_daemon_never_drains_stdin() {
        use std::io::Write as _;
        let mut script = std::env::temp_dir();
        script.push(format!("tars_stuck_daemon_{}.mjs", std::process::id()));
        {
            let mut f = std::fs::File::create(&script).unwrap();
            // No stdin listener, no stdout write — just keep the event loop
            // alive so the child never drains the request pipe.
            writeln!(
                f,
                "process.stderr.write('claude-daemon stdio ready (stuck-test)\\n'); \
                 setInterval(() => {{}}, 1e9);"
            )
            .unwrap();
        }
        let p = ClaudeSdkProviderBuilder::new("stuck_daemon_test")
            .default_model("m")
            .script_path(script.to_str().unwrap().to_string())
            .timeout(Duration::from_millis(800))
            .build();
        // Comfortably past the 64 KB pipe buffer so `write_all` blocks against
        // the non-draining child.
        let big_prompt = "x".repeat(256 * 1024);
        let req = ChatRequest::user(ModelHint::Explicit("m".into()), big_prompt);
        let t0 = std::time::Instant::now();
        let r = p
            .complete(req, tars_types::RequestContext::test_default())
            .await;
        let elapsed = t0.elapsed();
        let _ = std::fs::remove_file(&script);
        // Pre-fix: hangs (the stuck write was outside the timeout). Post-fix:
        // the whole round-trip is under timeout → Err within ~the budget.
        assert!(r.is_err(), "a non-draining daemon must yield Err, not hang");
        assert!(
            elapsed < Duration::from_secs(8),
            "must time out near the 800ms budget, not hang; took {elapsed:?}"
        );
    }
}
