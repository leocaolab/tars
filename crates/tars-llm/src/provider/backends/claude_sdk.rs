//! Calls Claude through one long-lived Node child running `tools/claude-daemon/server.mjs
//! --stdio`, which embeds `@anthropic-ai/claude-agent-sdk` and keeps its login and prompt cache
//! warm. A fresh `claude -p` per call pays a Node cold start, an OAuth handshake and a large
//! system-prompt load, measured at 60-300 s a call on realistic prompts.
//!
//! Concurrent requests share the child's stdin and stdout, one JSON object per line; each carries
//! an `id`, and a reader task hands each reply to the call waiting on that id. The child is
//! spawned on the first call and killed when the provider is dropped. A call that loses the child
//! (its stdin write fails, or the child exits before replying) or times out before its request is
//! written drops the session, and the next call spawns a new child. A call that times out waiting
//! for its reply, or gets a reply carrying the daemon's `error`, keeps it, since the child is still
//! there to answer the next call.
//!
//! ```json
//! stdin:  { "id": u64, "prompt": str, "system"?: str, "model"?: str, "max_turns": int,
//!           "schema"?: {...}, "thinking"?: "off" }
//! stdout: { "id": u64, "text"?: str, "thinking"?: str, "usage"?: {...}, "model"?: str,
//!           "result_subtype"?: str, "error"?: str }
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

use crate::ProviderError;
use crate::provider::backends::cli::is_stripped_env;
use crate::provider::error::{DaemonScriptError, IoOp, ParseProblem, RequestRefusal};
use crate::provider::http_base::truncate;
use tars_types::{
    Caller, ChatEvent, ChatRequest, ContentBlock, Message, ProviderId, ProviderProfile, StopReason,
    Usage,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// The calls waiting for a reply, by wire `id`. The reader removes an entry when its reply
/// arrives; a call that fails or times out removes its own.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<DaemonChatReply, ProviderError>>>>>;

pub struct ClaudeSdkProviderBuilder {
    id: ProviderId,
    executable: String,
    script_path: Option<String>,
    timeout: Duration,
    capabilities: Option<ProviderProfile>,
}

impl ClaudeSdkProviderBuilder {
    pub fn new(id: ProviderId) -> Self {
        Self {
            id,
            executable: tars_config::providers::CLAUDE_SDK_EXECUTABLE.into(),
            script_path: None,
            timeout: Duration::from_secs(tars_config::providers::CLAUDE_SDK_TIMEOUT_SECS),
            capabilities: None,
        }
    }

    builder_setter!(executable: into String);
    builder_setter!(script_path: into_opt String);
    builder_setter!(timeout: Duration);
    builder_setter!(capabilities: opt ProviderProfile);

    pub fn build(self) -> Arc<ClaudeSdkProvider> {
        // A missing daemon script fails the first call, not the build.
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        Arc::new(ClaudeSdkProvider {
            id: self.id,
            executable: self.executable,
            script_path: self.script_path,
            timeout: self.timeout,
            capabilities: caps,
            session: Mutex::new(None),
        })
    }
}

/// Returns the capabilities of the `claude_sdk` block of `data/provider.toml`.
fn default_capabilities() -> ProviderProfile {
    tars_config::capabilities_for("claude_sdk", "")
}

pub struct ClaudeSdkProvider {
    id: ProviderId,
    executable: String,
    script_path: Option<String>,
    timeout: Duration,
    capabilities: ProviderProfile,
    /// The running child; `None` until the first call, and again after a call lost it.
    session: Mutex<Option<Arc<Session>>>,
}

#[async_trait]
impl LlmProvider for ClaudeSdkProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &ProviderProfile {
        &self.capabilities
    }

    // `err(Display)` logs each failed call once, with the provider id and model.
    #[tracing::instrument(
        name = "claude_sdk.stream",
        skip_all,
        fields(
            provider = %self.id,
            model = %model,
        ),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        let resp = self.call(&req, model, &ctx).await?;
        // The daemon answers with one object, replayed as `Started`, deltas and `Finished`. A
        // reply without a result subtype comes from an SDK run that ended before its result
        // message, so it says no stop reason and gets no `Finished`.
        let end = ChatEvent::stream_end(
            resp.result_subtype.as_deref().map(map_stop_reason),
            resp.usage.as_ref().map(normalize_usage),
        );
        let mut events: Vec<Result<ChatEvent, ProviderError>> = vec![Ok(ChatEvent::Started {
            actual_model: resp.model,
            cache_hit: tars_types::CacheHitInfo::default(),
        })];
        if !resp.thinking.is_empty() {
            events.push(Ok(ChatEvent::ThinkingDelta {
                text: resp.thinking,
            }));
        }
        events.push(Ok(ChatEvent::Delta { text: resp.text }));
        events.extend(end.map(Ok));
        Ok(Box::pin(stream::iter(events)))
    }
}

impl ClaudeSdkProvider {
    async fn call(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &Caller,
    ) -> Result<DaemonChatReply, ProviderError> {
        let prompt = serialize_messages(req);
        if prompt.is_empty() {
            return Err(ProviderError::InvalidRequest(RequestRefusal::NoContent));
        }

        let session = self.ensure_session().await?;
        // Wraps only after 2^64 calls to one child.
        let id = session.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Result<DaemonChatReply, ProviderError>>();
        session.pending.lock().await.insert(id, tx);

        let body = ChatLine {
            id,
            prompt: &prompt,
            system: req.system.as_deref(),
            model: Some(model),
            // 7 covers three think-and-refine rounds plus the answer; extended thinking counts
            // a thinking block and its text block as two turns, and a lower ceiling fails with
            // "Reached maximum number of turns". The daemon disables tools, so more turns never
            // mean an agent loop.
            max_turns: 7,
            schema: req.structured_output.as_ref().map(|s| s.schema()),
            // Only `Off` turns thinking off; a budget is not passed on.
            thinking: (req.thinking == Some(tars_types::ThinkingMode::Off)).then_some("off"),
        };
        let mut line = serde_json::to_string(&body).map_err(ProviderError::Encode)?;
        line.push('\n');

        // The timeout covers the stdin write too: a daemon busy with another reply stops
        // draining stdin, the pipe fills, and `write_all` blocks (a call once hung 40 minutes
        // there). The caller's deadline wins over the configured timeout.
        let budget = ctx.call_budget(self.timeout);
        let deadline = tokio::time::Instant::now() + budget;
        let exchange = exchange(&session, &line, rx, deadline).await;
        if !matches!(exchange, Exchange::Replied(_)) {
            session.pending.lock().await.remove(&id);
        }
        if !keeps_session(&exchange) {
            self.clear_session(&session).await;
        }
        let timed_out = |phase: &str| ProviderError::TimedOut {
            budget,
            detail: format!(
                "claude_sdk: timed out {phase} (model={model}, prompt_chars={})",
                prompt.len()
            ),
        };
        match exchange {
            Exchange::Replied(reply) => reply,
            Exchange::Lost(e) => Err(e),
            Exchange::TimedOutWriting => Err(timed_out("writing the request")),
            Exchange::TimedOutWaiting => Err(timed_out("waiting for the reply")),
        }
    }

    /// Returns the live session, spawning it under the session lock, so concurrent first calls
    /// spawn one child.
    async fn ensure_session(&self) -> Result<Arc<Session>, ProviderError> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let s = Arc::new(self.spawn_session().await?);
        *guard = Some(s.clone());
        Ok(s)
    }

    /// Drops the session only if it is still `dead`; another call may already have replaced it
    /// with a healthy one.
    async fn clear_session(&self, dead: &Arc<Session>) {
        let mut guard = self.session.lock().await;
        if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, dead)) {
            *guard = None;
        }
    }

    async fn spawn_session(&self) -> Result<Session, ProviderError> {
        let resolved = match self.script_path.as_deref() {
            Some(p) => std::path::PathBuf::from(p),
            None => find_default_script_path()
                .ok_or(ProviderError::DaemonScript(DaemonScriptError::NotFound))?,
        };
        let script: &str = resolved.to_str().ok_or_else(|| {
            ProviderError::DaemonScript(DaemonScriptError::NotUtf8 {
                path: resolved.clone(),
            })
        })?;

        let mut cmd = daemon_command(
            &self.executable,
            script,
            std::env::vars_os().map(|(name, _)| name),
        );
        let mut child = cmd.spawn().map_err(|source| ProviderError::Spawn {
            program: self.executable.clone().into(),
            source,
        })?;

        let reaper_guard = child
            .id()
            .map(crate::provider::child_reaper::ReaperGuard::new);

        let stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Internal("claude_sdk: child stdin pipe missing".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::Internal("claude_sdk: child stdout pipe missing".into())
        })?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

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
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "claude_sdk reader: stdout read error");
                        break;
                    }
                }
            }
            // The child is gone: dropping the senders fails every waiting call now, as lost,
            // instead of at its timeout.
            pending_for_reader.lock().await.clear();
        });

        Ok(Session {
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(0),
            _child: Mutex::new(Some(child)),
            _reaper_guard: reaper_guard,
        })
    }
}

/// Returns the daemon's command: `script --stdio` under `executable`, piped, in its own process
/// group, with every variable of `inherited` that would make the SDK bill an API account instead
/// of the subscription removed, the same ones the claude CLI is never given.
fn daemon_command(
    executable: &str,
    script: &str,
    inherited: impl Iterator<Item = std::ffi::OsString>,
) -> Command {
    let mut cmd = Command::new(executable);
    cmd.args([script, "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The daemon's ready line and SDK warnings reach the operator's terminal.
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    for name in inherited.filter(|name| name.to_str().is_some_and(is_stripped_env)) {
        cmd.env_remove(name);
    }
    // Its own process group, so the signal-time reaper kills the daemon and the claude
    // children it spawns together; a SIGINT or SIGTERM to tars runs no `kill_on_drop`.
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// How one call's exchange with the child ended.
#[derive(Debug)]
enum Exchange {
    /// The daemon answered, with its answer or its own error.
    Replied(Result<DaemonChatReply, ProviderError>),
    /// The child is gone: the stdin write failed, or it exited before replying.
    Lost(ProviderError),
    /// The call's budget ran out before its request line was written whole.
    TimedOutWriting,
    /// The call's budget ran out after its request line was written, while the daemon worked.
    TimedOutWaiting,
}

/// Returns whether the session survives an exchange. A daemon that stopped draining stdin would
/// hang every later call too, and a half-written line corrupts the next one, so a timeout while
/// writing drops it. A timeout while waiting is this call's deadline only: the daemon answers
/// requests concurrently and drops a reply whose caller left.
fn keeps_session(exchange: &Exchange) -> bool {
    match exchange {
        Exchange::Replied(_) | Exchange::TimedOutWaiting => true,
        Exchange::Lost(_) | Exchange::TimedOutWriting => false,
    }
}

/// Writes one request line and waits for its reply, both before `deadline`.
async fn exchange(
    session: &Session,
    line: &str,
    rx: oneshot::Receiver<Result<DaemonChatReply, ProviderError>>,
    deadline: tokio::time::Instant,
) -> Exchange {
    let write = async {
        let mut stdin = session.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|source| ProviderError::Io {
                op: IoOp::WriteChildStdin,
                source,
            })?;
        stdin.flush().await.map_err(|source| ProviderError::Io {
            op: IoOp::FlushChildStdin,
            source,
        })
    };
    match tokio::time::timeout_at(deadline, write).await {
        Err(_) => return Exchange::TimedOutWriting,
        Ok(Err(e)) => return Exchange::Lost(e),
        Ok(Ok(())) => {}
    }
    match tokio::time::timeout_at(deadline, rx).await {
        Err(_) => Exchange::TimedOutWaiting,
        Ok(Ok(reply)) => Exchange::Replied(reply),
        // The reader drops every waiting sender when the child's stdout ends.
        Ok(Err(_)) => Exchange::Lost(ProviderError::DaemonExited),
    }
}

async fn dispatch_reply_line(line: &str, pending: PendingMap) -> Result<(), ProviderError> {
    let unparsed = |problem| ProviderError::Parse {
        at: "claude_sdk daemon reply",
        line: None,
        problem: Box::new(problem),
        raw: Some(truncate(line, 200)),
    };
    let raw: ReplyLine = serde_json::from_str(line).map_err(|e| unparsed(ParseProblem::Json(e)))?;
    let id = raw.id;
    // A reply with neither `error` nor `text` is a daemon bug, not an empty answer.
    let result: Result<DaemonChatReply, ProviderError> = match (raw.error, raw.text) {
        (Some(message), _) => Err(ProviderError::DaemonFailed { message }),
        (None, Some(text)) => Ok(DaemonChatReply {
            text,
            thinking: raw.thinking.unwrap_or_default(),
            result_subtype: raw.result_subtype,
            usage: raw.usage,
            model: raw.model,
        }),
        (None, None) => Err(unparsed(ParseProblem::Missing("text"))),
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
    /// The child, held so dropping the session kills it; the mutex only makes it `Sync`.
    _child: Mutex<Option<tokio::process::Child>>,
    /// The reaper registration, held so dropping the session removes the daemon from the
    /// signal-time reaper; `None` when the OS gave no PID.
    _reaper_guard: Option<crate::provider::child_reaper::ReaperGuard>,
}

/// Returns the first daemon script that exists among [`default_script_paths`] of the working
/// directory and `$HOME`.
fn find_default_script_path() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok();
    let home = tars_types::env::home_dir();
    default_script_paths(cwd.as_deref(), home.as_deref())
        .into_iter()
        .find(|candidate| candidate.exists())
}

/// Returns where an unconfigured daemon script is looked for, in order:
/// `tools/claude-daemon/server.mjs` in `cwd` and each of its ancestors, then
/// `<home>/.tars/claude-daemon/server.mjs`, where `tars install-claude-daemon` puts it.
fn default_script_paths(
    cwd: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let in_checkout = cwd
        .into_iter()
        .flat_map(std::path::Path::ancestors)
        .map(|dir| dir.join("tools/claude-daemon/server.mjs"));
    let installed = home.map(|home| home.join(".tars/claude-daemon/server.mjs"));
    in_checkout.chain(installed).collect()
}

/// Returns the messages as one prompt, each under a `[role]` header; a message with no content
/// is skipped.
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

/// Returns the tars usage of an Anthropic-shaped one, whose input count excludes the cached
/// and cache-creation tokens that [`Usage::input_tokens`] includes.
fn normalize_usage(u: &DaemonUsage) -> Usage {
    let cached = u.cache_read_input_tokens.unwrap_or(0);
    let created = u.cache_creation_input_tokens.unwrap_or(0);
    let fresh = u.input_tokens.unwrap_or(0);
    Usage {
        input_tokens: fresh.saturating_add(cached).saturating_add(created),
        output_tokens: u.output_tokens.unwrap_or(0),
        cached_input_tokens: cached,
        cache_creation_tokens: created,
        ..Default::default()
    }
}

fn map_stop_reason(subtype: &str) -> StopReason {
    match subtype {
        "success" | "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        _ => StopReason::Other,
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
    /// The structured-output schema; the daemon passes it as the SDK's `outputFormat` and
    /// returns the constrained answer as the reply text.
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<&'a serde_json::Map<String, serde_json::Value>>,
    /// `"off"` turns extended thinking off; absent, the daemon thinks and returns the
    /// summarized reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<&'a str>,
}

/// A reply line of either shape: an `error` makes it a failure whatever else it holds.
#[derive(Debug, Deserialize)]
struct ReplyLine {
    id: u64,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    result_subtype: Option<String>,
    #[serde(default)]
    usage: Option<DaemonUsage>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug)]
struct DaemonChatReply {
    text: String,
    thinking: String,
    result_subtype: Option<String>,
    usage: Option<DaemonUsage>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DaemonUsage {
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

    #[test]
    fn an_unconfigured_daemon_script_is_looked_for_only_in_the_checkout_and_home() {
        let found = default_script_paths(
            Some(std::path::Path::new("/w/repo")),
            Some(std::path::Path::new("/h")),
        );

        assert_eq!(
            found,
            [
                "/w/repo/tools/claude-daemon/server.mjs",
                "/w/tools/claude-daemon/server.mjs",
                "/tools/claude-daemon/server.mjs",
                "/h/.tars/claude-daemon/server.mjs",
            ]
            .map(std::path::PathBuf::from)
        );
    }

    #[test]
    fn a_builder_and_a_block_that_set_nothing_run_the_same_daemon_with_the_same_timeout() {
        let cfg = tars_config::Config::from_toml(
            "[providers.sdk]\ntype = \"claude_sdk\"\ndefault_model = \"claude-sonnet-4-5\"\n",
        )
        .unwrap();
        let Some(tars_config::ProviderConfig::ClaudeSdk {
            executable,
            timeout_secs,
            ..
        }) = cfg.providers().get(&ProviderId::new("sdk"))
        else {
            panic!("the block is a claude_sdk block");
        };

        let built = ClaudeSdkProviderBuilder::new(ProviderId::new("sdk"));

        assert_eq!(built.timeout, Duration::from_secs(*timeout_secs));
        assert_eq!(&built.executable, executable);
    }

    #[test]
    fn a_daemon_error_reply_keeps_the_session_and_a_lost_child_drops_it() {
        let daemon_error = Exchange::Replied(Err(ProviderError::DaemonFailed {
            message: "rate limited".into(),
        }));
        assert!(keeps_session(&daemon_error));
        assert!(!keeps_session(&Exchange::Lost(ProviderError::DaemonExited)));
        assert!(!keeps_session(&Exchange::TimedOutWriting));
    }

    #[test]
    fn serialize_single_user_message() {
        let req = ChatRequest::user("ping");
        assert_eq!(serialize_messages(&req), "[user]\nping");
    }

    #[test]
    fn reply_line_carries_thinking() {
        let raw: ReplyLine = serde_json::from_str(
            r#"{"id":3,"text":"{\"findings\":[]}","thinking":"checked rule X, no match"}"#,
        )
        .unwrap();
        assert_eq!(raw.thinking.as_deref(), Some("checked rule X, no match"));
        let reply = DaemonChatReply {
            text: raw.text.unwrap_or_default(),
            thinking: raw.thinking.unwrap_or_default(),
            result_subtype: raw.result_subtype,
            usage: raw.usage,
            model: raw.model,
        };
        assert_eq!(reply.thinking, "checked rule X, no match");
    }

    #[test]
    fn reply_line_thinking_absent_is_empty() {
        let raw: ReplyLine = serde_json::from_str(r#"{"id":1,"text":"hi"}"#).unwrap();
        assert_eq!(raw.thinking, None);
    }

    #[test]
    fn normalize_usage_sums_into_input_total() {
        let raw = DaemonUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            cache_read_input_tokens: Some(900),
            cache_creation_input_tokens: Some(10),
        };
        let u = normalize_usage(&raw);
        assert_eq!(u.input_tokens, 1010); // 100 + 900 + 10
        assert_eq!(u.cached_input_tokens, 900);
        assert_eq!(u.cache_creation_tokens, 10);
        assert_eq!(u.output_tokens, 50);
    }

    #[test]
    fn daemon_usage_counts_past_u64_saturate_instead_of_wrapping() {
        let raw = DaemonUsage {
            input_tokens: Some(u64::MAX),
            output_tokens: None,
            cache_read_input_tokens: Some(1),
            cache_creation_input_tokens: Some(1),
        };
        assert_eq!(normalize_usage(&raw).input_tokens, u64::MAX);
    }

    #[test]
    fn map_stop_reason_known_subtypes() {
        assert_eq!(map_stop_reason("success"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("error_max_turns"), StopReason::Other);
        assert_eq!(map_stop_reason("nonsense"), StopReason::Other);
    }

    /// Needs node, the SDK and a login; run with
    /// `cargo test -p tars-llm --lib backends::claude_sdk -- --ignored`.
    #[tokio::test]
    #[ignore = "spawns node + uses live Anthropic OAuth"]
    async fn smoke_stdio_roundtrip() {
        // Finds the script by the default search, so it runs inside the checkout.
        let p = ClaudeSdkProviderBuilder::new(ProviderId::new("claude_sdk_smoke")).build();
        let req = ChatRequest::user("Reply with only the literal word: pong");
        let resp = p
            .clone()
            .complete(req, "test-model", tars_types::Caller::test_default())
            .await
            .expect("daemon round-trip");
        let text = resp.text.to_lowercase();
        assert!(
            text.contains("pong"),
            "expected reply to contain 'pong', got: {:?}",
            resp.text
        );
        assert!(resp.usage.unwrap().output_tokens >= 1);
    }

    #[tokio::test]
    #[ignore = "spawns node + uses live Anthropic OAuth"]
    async fn smoke_stdio_concurrent_requests() {
        let p =
            ClaudeSdkProviderBuilder::new(ProviderId::new("claude_sdk_smoke_concurrent")).build();
        let req_a = ChatRequest::user("Reply with only: alpha");
        let req_b = ChatRequest::user("Reply with only: beta");
        let p2 = p.clone();
        let p3 = p.clone();
        let (a, b) = tokio::join!(
            p2.complete(req_a, "test-model", tars_types::Caller::test_default()),
            p3.complete(req_b, "test-model", tars_types::Caller::test_default()),
        );
        let a = a.expect("alpha");
        let b = b.expect("beta");
        assert!(a.text.to_lowercase().contains("alpha"), "{:?}", a.text);
        assert!(b.text.to_lowercase().contains("beta"), "{:?}", b.text);
    }

    /// A daemon that never reads stdin (as when the SDK sleeps through a rate limit) makes a
    /// prompt larger than the 64 KB pipe buffer block in `write_all`; the call still times out.
    #[tokio::test]
    #[ignore = "spawns a node child; verifies timeout covers a stuck stdin write"]
    async fn call_times_out_when_daemon_never_drains_stdin() {
        use std::io::Write as _;
        let mut script = std::env::temp_dir();
        script.push(format!("tars_stuck_daemon_{}.mjs", std::process::id()));
        {
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(
                f,
                "process.stderr.write('claude-daemon stdio ready (stuck-test)\\n'); \
                 setInterval(() => {{}}, 1e9);"
            )
            .unwrap();
        }
        let p = ClaudeSdkProviderBuilder::new(ProviderId::new("stuck_daemon_test"))
            .script_path(script.to_str().unwrap().to_string())
            .timeout(Duration::from_millis(800))
            .build();
        let big_prompt = "x".repeat(256 * 1024);
        let req = ChatRequest::user(big_prompt);
        let t0 = std::time::Instant::now();
        let r = p
            .complete(req, "test-model", tars_types::Caller::test_default())
            .await;
        let elapsed = t0.elapsed();
        let _ = std::fs::remove_file(&script);
        assert!(r.is_err(), "a non-draining daemon must yield Err, not hang");
        assert!(
            elapsed < Duration::from_secs(8),
            "must time out near the 800ms budget, not hang; took {elapsed:?}"
        );
    }

    #[test]
    fn only_a_timeout_before_the_request_is_written_drops_the_session() {
        assert!(keeps_session(&Exchange::TimedOutWaiting));
        assert!(!keeps_session(&Exchange::TimedOutWriting));
    }

    #[test]
    fn the_daemon_is_not_given_the_variables_that_switch_claude_to_api_billing() {
        let cmd = daemon_command(
            "node",
            "server.mjs",
            ["Anthropic_Api_Key", "CLAUDE_CODE_USE_BEDROCK", "PATH"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        let removed: Vec<_> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_os_string())
            .collect();
        assert_eq!(removed, ["Anthropic_Api_Key", "CLAUDE_CODE_USE_BEDROCK"]);
    }
}
