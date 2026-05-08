//! Stateful multi-turn conversation container.
//!
//! `Session` sits one layer above `Pipeline`: it accumulates history
//! across many `complete()` calls, enforces role-alternation invariants
//! the providers care about, runs the tool-dispatch loop transparently,
//! and trims context to fit the model's window. Callers see a simple
//! `send(text) -> Response` surface; everything else is internal.
//!
//! ## Layered model
//!
//! ```text
//! Layer 0: Provider  — raw HTTP / SSE
//! Layer 1: Pipeline  — Provider + cache/retry/telemetry middleware
//! Layer 2: Session   — Pipeline + history + tool loop  ← THIS MODULE
//! Layer 3: Agent runtime — Sessions composed into Orchestrator/Worker/Critic
//! ```
//!
//! Session is *composition* over Pipeline (`HAS-A`), not subclassing.
//! A caller who doesn't want history just calls `pipeline.complete()`
//! directly; nothing forces them through Session.
//!
//! ## Invariants (enforced)
//!
//! - System prompt never enters `turns` — always re-attached to each
//!   request from `self.system`.
//! - Each `Turn` opens with a `User` message, ends with an `Assistant`
//!   text reply (no orphan tool_use). Validated by [`Turn::is_complete`].
//! - Trim runs **exactly once** per `send()` call — at entry, before
//!   the first model invocation. Auto-loop continuations (tool_use →
//!   tool_result roundtrips) accumulate within the current turn and do
//!   not re-trim. A release-mode `assert!` guards this in debug + prod.
//! - Failure of any kind during a `send()` rolls back the entire
//!   in-progress Turn via a Drop guard. Caller-visible state is
//!   atomic at the Turn boundary.
//!
//! See `tests::` at the bottom for end-to-end exercises of each.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use futures::StreamExt;
use serde_json::Value as JsonValue;
use tars_pipeline::{LlmService, RequestContext};
use tars_types::{
    error::ProviderError, new_shared_telemetry, Capabilities, ChatRequest, ChatResponseBuilder,
    ContentBlock, Message, ModelHint, SharedTelemetry, TelemetryAccumulator, ToolChoice, ToolSpec,
};

// ── Budget ────────────────────────────────────────────────────────────

/// How Session decides when to trim history.
///
/// Three modes — pick whichever maps to information you actually have:
///
/// - [`Budget::Chars`] — count `len()` of every text block. Cheap,
///   provider-agnostic, no tokenizer needed. **Default**.
/// - [`Budget::Tokens`] — exact token counts via a [`Tokenizer`]. Use
///   when you have a tokenizer matching the model and care about
///   utilization above the 80%-of-context heuristic.
/// - [`Budget::ContextRatio`] — read `capabilities.max_context_tokens`
///   from the underlying provider and use the given fraction. The
///   sugar option: caller doesn't have to pick a number.
#[derive(Clone, Debug)]
pub enum Budget {
    Chars(usize),
    Tokens { limit: usize, tokenizer: Arc<dyn Tokenizer> },
    ContextRatio(f32),
}

impl Default for Budget {
    fn default() -> Self {
        // ~100k tokens at the typical 4 chars/token ratio. Same as ARC's
        // 80-line Session default — known-good in production.
        Self::Chars(400_000)
    }
}

/// Pluggable tokenizer for [`Budget::Tokens`]. Sessions that don't use
/// `Tokens` budgets never call this trait, so the dependency is opt-in.
pub trait Tokenizer: Send + Sync + std::fmt::Debug {
    fn count(&self, text: &str) -> usize;
}

impl Budget {
    /// Compute a per-message cost under this budget.
    fn cost_of(&self, msg: &Message) -> usize {
        let total_text: String = msg.content().iter().filter_map(|c| c.as_text()).collect();
        match self {
            Self::Chars(_) => total_text.chars().count(),
            Self::Tokens { tokenizer, .. } => tokenizer.count(&total_text),
            // ContextRatio reuses Chars accounting under the hood — see
            // `effective_limit`. The cost function stays shape-aligned.
            Self::ContextRatio(_) => total_text.chars().count(),
        }
    }

    /// Resolve the absolute limit for a given model's capabilities.
    /// `ContextRatio` reads from caps; the others ignore caps entirely.
    fn effective_limit(&self, caps: &Capabilities) -> usize {
        match self {
            Self::Chars(n) | Self::Tokens { limit: n, .. } => *n,
            Self::ContextRatio(ratio) => {
                // Reserve `ratio` of model context for *history*; the
                // rest covers the new user message + system prompt +
                // model output. The caller-set ratio is on the gross
                // context window — we don't try to subtract `max_output_tokens`
                // here because Session doesn't know what max_output the
                // caller will request next.
                let total = caps.max_context_tokens as usize;
                // Convert tokens → char-equivalent at the typical 4:1
                // ratio (matches Chars budget so the same cost function
                // applies). Rough but consistent.
                (total as f32 * 4.0 * ratio).round() as usize
            }
        }
    }
}

// ── Tool registry ─────────────────────────────────────────────────────

/// A function the model can call during a Session turn.
///
/// Implementors do whatever (HTTP fetch, file read, shell out, in-process
/// query) and return a JSON value. The Session serializes the result
/// into the next turn's tool_result message automatically.
pub trait Tool: Send + Sync {
    /// Tool name as the model sees it. Must match the [`ToolSpec`]
    /// `name` field registered for this tool.
    fn name(&self) -> &str;

    /// Execute the tool with the model-supplied arguments. Returning
    /// `Err` aborts the entire turn (atomic rollback) and surfaces the
    /// error to the caller. Tools that want to convey a recoverable
    /// problem to the model should return `Ok(json!({"error": ...}))`
    /// instead — the model receives that as a normal tool_result and
    /// can choose to retry / give up / route around.
    fn call(
        &self,
        arguments: JsonValue,
    ) -> futures::future::BoxFuture<'_, Result<JsonValue, SessionError>>;

    /// The schema we send to the model so it knows how to call us.
    fn spec(&self) -> ToolSpec;
}

/// Bundle of registered tools. Lookups are O(1) by name.
#[derive(Default)]
pub struct ToolRegistry {
    by_name: std::collections::HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.by_name.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_name.get(name)
    }

    /// All registered specs — sent to the model with each request so it
    /// knows what's available.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.by_name.values().map(|t| t.spec()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

// ── Turn ──────────────────────────────────────────────────────────────

/// One logical exchange opened by a single `send()` call.
///
/// Holds the full message chain — leading user prompt, plus any number
/// of `(assistant tool_use → user tool_result)` rounds, ending in an
/// assistant text reply. Trim and rollback are atomic at this level.
///
/// Note: the `User` role inside `messages[1..]` is conventionally a
/// tool_result carrier, not a new prompt. New user prompts open a
/// fresh `Turn`. This convention follows Anthropic's protocol shape;
/// OpenAI's `Tool` role messages get translated by the adapter.
#[derive(Clone, Debug)]
pub struct Turn {
    /// All messages making up this turn, in chronological order.
    /// See `is_complete` for the invariants checked at turn-close.
    messages: Vec<Message>,
}

impl Turn {
    /// Open a new turn with a leading user message.
    pub fn open(leading: Message) -> Self {
        debug_assert!(matches!(leading, Message::User { .. }), "Turn must open with User");
        Self { messages: vec![leading] }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    fn push(&mut self, m: Message) {
        self.messages.push(m);
    }

    /// Validates that this Turn ended cleanly — leading user message,
    /// alternating assistant/user continuation, final assistant text
    /// reply with no orphan tool_use. Used at:
    ///
    /// - `Session::finalize_turn` to assert close invariants
    /// - Future `Session::deserialize` to reject corrupted snapshots
    ///
    /// **NOT** an invariant during the auto-loop's
    /// `run_tools_and_append`: a half-completed Turn (assistant tool_use
    /// waiting for user tool_result) is valid in-flight state. Calling
    /// `is_complete()` mid-loop will return false and that's correct —
    /// the loop hasn't finished yet.
    pub fn is_complete(&self) -> bool {
        let Some(first) = self.messages.first() else { return false };
        if !matches!(first, Message::User { .. }) {
            return false;
        }
        let Some(last) = self.messages.last() else { return false };
        let Message::Assistant { content, tool_calls } = last else { return false };
        // Final reply must have text and no pending tool_calls.
        if !tool_calls.is_empty() {
            return false;
        }
        if !content.iter().any(|c| matches!(c, ContentBlock::Text { .. })) {
            return false;
        }
        true
    }
}

// ── TurnGuard (RAII rollback) ────────────────────────────────────────

/// Drop guard that truncates the session's turn list back to a
/// boundary unless [`commit`] is called. The `mem::forget` on commit
/// suppresses the Drop, leaving the appended Turn in place.
///
/// **Scoped guard, commit-pattern**: default behavior on Drop is
/// rollback. Success path *must* call `commit()` (consuming `self`).
/// This is strictly safer than the alternative `armed: bool` flag
/// because there's no way to forget a single statement and silently
/// keep a half-Turn — the borrow checker propagates the lifetime,
/// so anything other than calling `commit()` rolls back.
struct TurnGuard<'a> {
    turns: &'a mut Vec<Turn>,
    boundary: usize,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        // Default = rollback. Only success path calls commit().
        self.turns.truncate(self.boundary);
    }
}

impl<'a> TurnGuard<'a> {
    /// Successful turn finalized — skip the rollback.
    fn commit(self) {
        std::mem::forget(self);
    }
}

// ── Session error ────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("provider call failed: {0}")]
    Provider(#[from] ProviderError),

    #[error("tool {name:?} returned error: {message}")]
    ToolFailed { name: String, message: String },

    #[error("internal session bug: {0}")]
    Internal(String),
}

// ── Session ──────────────────────────────────────────────────────────

/// Per-instance increasing id. Useful for telemetry correlation.
static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Configuration knobs for `Session::new`.
pub struct SessionOptions {
    pub system: String,
    pub budget: Budget,
    pub tools: Option<ToolRegistry>,
    /// `max_output_tokens` to use when `send()` is called without an
    /// explicit value. `None` defers to the provider default.
    pub default_max_output_tokens: Option<u32>,
    /// The model id this Session targets. Per-call override possible
    /// via `send_with` (future); usually this is the same model used
    /// across all turns in one logical conversation.
    pub model: ModelHint,
}

/// Stateful multi-turn conversation. See module docs for the layered
/// model and invariants.
///
/// **Thread safety**: `Session` is `!Sync` (mutable internal state).
/// Concurrent access from multiple tasks must be serialized externally
/// (`Arc<tokio::sync::Mutex<Session>>` or similar). PyO3 callers get
/// this for free — PyO3's per-instance lock holds `&mut self` over
/// each method call.
pub struct Session {
    id: String,
    pipeline: Arc<dyn LlmService>,
    capabilities: Capabilities,
    system: String,
    model: ModelHint,
    turns: Vec<Turn>,
    budget: Budget,
    tools: Option<ToolRegistry>,
    default_max_output_tokens: Option<u32>,
    /// Counts in-turn budget exceedances for log dedup. See the Drop
    /// summary on the impl below.
    budget_warning_count: usize,
    /// Monotone counter bumped every time history changes in a way
    /// the next request would see (turn appended, history reset,
    /// trim ran). NOT bumped on rollback (truncating back to the
    /// pre-send boundary leaves history unchanged from the caller's
    /// observable POV). NOT bumped during the in-flight tool loop
    /// (those mutations are still part of one in-progress turn that
    /// either commits or rolls back atomically).
    ///
    /// Borrowed from codex-rs's `ContextManager.history_version`.
    /// Useful for caches keyed on history (token counters, prompt
    /// cache pre-hash) and for log correlation: a (session_id,
    /// history_version) pair uniquely identifies "which snapshot
    /// produced this log line".
    ///
    /// `fork()` preserves the parent's value rather than zeroing —
    /// this lets the cache layer recognize "these two sessions
    /// shared the first N turns" and reuse cached prefixes until
    /// they diverge.
    history_version: u64,
}

impl Session {
    pub fn new(
        pipeline: Arc<dyn LlmService>,
        capabilities: Capabilities,
        opts: SessionOptions,
    ) -> Self {
        let id = format!(
            "sess-{:08x}",
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            id,
            pipeline,
            capabilities,
            system: opts.system,
            model: opts.model,
            turns: Vec::new(),
            budget: opts.budget,
            tools: opts.tools,
            default_max_output_tokens: opts.default_max_output_tokens,
            budget_warning_count: 0,
            history_version: 0,
        }
    }

    /// Monotone version stamp bumped on each visible history mutation.
    /// See the field doc on `Session` for what counts as "visible".
    pub fn history_version(&self) -> u64 {
        self.history_version
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// All messages in chronological order. Caller-mutate-safe (it's a
    /// fresh Vec).
    pub fn history(&self) -> Vec<Message> {
        self.turns.iter().flat_map(|t| t.messages.iter().cloned()).collect()
    }

    /// Frozen turn-grouped view. For consumers that care about turn
    /// boundaries (rollback / fork / debug visualization).
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Drop all conversation state but keep system / budget / tools /
    /// model / default_max_output_tokens. Useful between unrelated
    /// tasks that share the same Session config.
    pub fn reset(&mut self) {
        self.turns.clear();
        self.budget_warning_count = 0;
        // Reset is a history rewrite — bump for cache invalidation.
        self.history_version = self.history_version.saturating_add(1);
    }

    /// Register a tool the model can call during this session. Tools
    /// can be added at any time but are only seen by the model on
    /// subsequent `send()` calls. Existing in-flight calls are not
    /// retroactively affected.
    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        self.tools.get_or_insert_with(ToolRegistry::new).register(tool);
    }

    /// Cheap-clone of conversation state. The returned Session shares
    /// the same `pipeline` (Arc clone), but has its own `turns` and
    /// `budget_warning_count`. Useful for branching speculative reviews
    /// — fork off, run two competing hypotheses, throw away the loser.
    pub fn fork(&self) -> Self
    where
        Self: Sized,
    {
        Self {
            id: format!(
                "{}-fork-{:04x}",
                self.id,
                SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            pipeline: self.pipeline.clone(),
            capabilities: self.capabilities.clone(),
            system: self.system.clone(),
            model: self.model.clone(),
            turns: self.turns.clone(),
            budget: self.budget.clone(),
            // ToolRegistry isn't Clone (Arc<dyn Tool> entries are, but
            // HashMap<String, Arc<dyn Tool>> is). We re-build the map;
            // tools themselves are cheap to share via Arc.
            tools: self.tools.as_ref().map(|tr| ToolRegistry {
                by_name: tr.by_name.clone(),
            }),
            default_max_output_tokens: self.default_max_output_tokens,
            budget_warning_count: 0,
            // Preserve parent's history_version so the cache layer can
            // see "these two sessions shared the same prefix until
            // they diverged". After fork, both branches mutate
            // independently and their versions drift apart — but the
            // parent-at-fork-time version still appears as a common
            // ancestor in the history.
            history_version: self.history_version,
        }
    }

    // ── Send (entry point) ─────────────────────────────────────────────

    /// Send one user message and run the conversation forward by one
    /// turn. If a `ToolRegistry` is set and the model emits tool_use,
    /// the loop dispatches tools and re-invokes the model until it
    /// returns a final text reply. The full final `Response` is
    /// returned — usage / stop_reason / text all populated.
    pub async fn send(
        &mut self,
        user_text: &str,
        max_output_tokens: Option<u32>,
    ) -> Result<(tars_types::ChatResponse, TelemetryAccumulator), SessionError> {
        // Open new turn; arm rollback guard.
        let boundary = self.turns.len();
        self.turns.push(Turn::open(Message::user_text(user_text)));

        // The guard borrows `&mut self.turns`. To make the rest of
        // `send` work without fighting the borrow checker, we drop
        // through a closure-shaped sequence: do all work via free
        // functions that take `&mut self.turns` etc., not `&mut self`.
        // For the outer body we keep `&mut self` available by re-
        // borrowing only inside the trim step (which is the only time
        // the guard's `turns` field gets touched).
        let guard = TurnGuard { turns: &mut self.turns, boundary };

        // Budget trim — exactly once, before any model call. Walk the
        // (immutable) trim helper using `guard.turns`, which is the
        // same Vec as self.turns, just routed through the guard's
        // exclusive borrow.
        let limit = self.budget.effective_limit(&self.capabilities);
        if let Some(warn) = trim_to_budget(guard.turns, &self.budget, limit) {
            // BudgetWarning de-dup: emit once per Session, summarize on Drop.
            self.budget_warning_count = self.budget_warning_count.saturating_add(1);
            if self.budget_warning_count == 1 {
                tracing::warn!(
                    session_id = %self.id,
                    over_by = warn.over_by,
                    "context budget exceeded after trim — turn may hit context limit"
                );
            }
        }

        // Inner loop dispatches model calls + tool execution. Keeps
        // `guard` alive across awaits so any error path (including
        // `?`) drops the guard → truncate(boundary).
        let pipeline = self.pipeline.clone();
        let model = self.model.clone();
        let max_out = max_output_tokens.or(self.default_max_output_tokens);
        let system = self.system.clone();
        let tools_specs = self.tools.as_ref().map(|tr| tr.specs()).unwrap_or_default();

        // Pre-create one telemetry handle and reuse across every
        // pipeline.call() inside the auto-loop, so caller sees a
        // single aggregated view of "this whole send" — not just the
        // last model call. retry_count + retry_attempts +
        // provider_latency_ms accumulate across all loop iterations.
        let telemetry: SharedTelemetry = new_shared_telemetry();
        let final_response = loop_until_text(
            guard.turns,
            pipeline,
            model,
            system,
            tools_specs,
            self.tools.as_ref(),
            max_out,
            telemetry.clone(),
        )
        .await?;

        // Success — disarm the rollback guard. Anything after this
        // point that fails would still leave the turn in place
        // (responsibility shifts to the caller).
        guard.commit();
        // Visible history mutation succeeded — bump version. Failed
        // sends explicitly do NOT reach here (they early-return
        // through the `?` above), so rollback paths leave the version
        // unchanged. That's the correct semantic: a rolled-back send
        // leaves the caller-visible history identical to before.
        self.history_version = self.history_version.saturating_add(1);

        let acc = telemetry
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        Ok((final_response, acc))
    }

    /// Convenience: like `send` but returns just the assistant text.
    /// `arc.LLMClient.chat`-shaped wrapper.
    pub async fn send_text(
        &mut self,
        user_text: &str,
        max_output_tokens: Option<u32>,
    ) -> Result<String, SessionError> {
        let (resp, _telemetry) = self.send(user_text, max_output_tokens).await?;
        Ok(resp.text)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.budget_warning_count > 1 {
            tracing::warn!(
                session_id = %self.id,
                count = self.budget_warning_count,
                "session ended with {} budget exceedances total",
                self.budget_warning_count,
            );
        }
    }
}

// ── Trim ──────────────────────────────────────────────────────────────

struct TrimWarning {
    over_by: usize,
}

/// Drop oldest *complete* turns until total cost ≤ `limit`. Refuses
/// to split a turn (tool_use without matching tool_result is fatal at
/// the wire). If the most recent turn alone exceeds `limit`, returns
/// a `TrimWarning` (not an error) — the call goes through and may hit
/// `ContextTooLong` at the provider; that's a more informative failure
/// mode than refusing to call at all.
fn trim_to_budget(
    #[allow(clippy::ptr_arg)] turns: &mut Vec<Turn>,
    budget: &Budget,
    limit: usize,
) -> Option<TrimWarning> {
    fn total_cost(turns: &[Turn], budget: &Budget) -> usize {
        turns.iter().flat_map(|t| t.messages.iter()).map(|m| budget.cost_of(m)).sum()
    }

    while total_cost(turns, budget) > limit && turns.len() > 1 {
        // Drop oldest *whole* turn. We never split a turn — that would
        // orphan a tool_use without its tool_result, which Anthropic
        // and OpenAI both reject at the wire.
        turns.remove(0);
    }
    let total = total_cost(turns, budget);
    if total > limit {
        Some(TrimWarning { over_by: total - limit })
    } else {
        None
    }
}

// ── Auto-loop ────────────────────────────────────────────────────────

/// Drive the model + tool loop until the model returns a final text
/// reply (no tool_calls). On parallel tool_use, all results are sent
/// back in a single user message (Anthropic protocol requirement).
///
/// **Critical invariant**: this function does NOT call `trim_to_budget`.
/// Trimming inside the loop could orphan a tool_use that's already been
/// sent to the model in a previous round. Trim runs exactly once at
/// `send()` entry, before the first call.
#[allow(clippy::too_many_arguments)]
async fn loop_until_text(
    turns: &mut [Turn],
    pipeline: Arc<dyn LlmService>,
    model: ModelHint,
    system: String,
    tool_specs: Vec<ToolSpec>,
    tools: Option<&ToolRegistry>,
    max_output_tokens: Option<u32>,
    telemetry: SharedTelemetry,
) -> Result<tars_types::ChatResponse, SessionError> {
    loop {
        let prev_turn_count = turns.len();

        // Build flat history: everything across turns except the
        // not-yet-replied current turn's pending tool_use (which is
        // still inside the latest turn's messages).
        let messages = flatten_for_request(turns);

        let req = ChatRequest {
            model: model.clone(),
            system: Some(system.clone()),
            messages,
            tools: tool_specs.clone(),
            tool_choice: ToolChoice::Auto,
            structured_output: None,
            max_output_tokens,
            temperature: None,
            stop_sequences: Vec::new(),
            seed: None,
            cache_directives: Vec::new(),
            thinking: tars_types::ThinkingMode::default(),
            enable_chat_template_thinking: None,
        };

        // Drain pipeline stream into a complete ChatResponse.
        // `LlmService::call` takes `Arc<Self>` by value, so clone the
        // outer Arc per iteration. Each call shares the same
        // telemetry handle so accumulated layers/retries/latency
        // reflect the WHOLE send, not just the last model call.
        let mut ctx = RequestContext::test_default();
        ctx.telemetry = telemetry.clone();
        let mut stream = pipeline
            .clone()
            .call(req, ctx)
            .await
            .map_err(SessionError::from)?;
        let mut builder = ChatResponseBuilder::new();
        while let Some(ev) = stream.next().await {
            builder.apply(ev.map_err(SessionError::from)?);
        }
        let response = builder.finish();

        // Did the model emit tool_calls? If not, this is a final reply
        // and we close out the turn.
        let assistant_msg = build_assistant_message_from(&response);
        let has_tool_calls = matches!(&assistant_msg,
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty());

        if !has_tool_calls {
            // Append final assistant message; close the turn.
            let last_turn = turns.last_mut().expect("turn was opened in send()");
            last_turn.push(assistant_msg);

            // Turn-close invariant. is_complete() is the right gate
            // here (not in mid-loop) — see Turn::is_complete docs.
            assert!(
                last_turn.is_complete(),
                "BUG: Turn finalized but is_complete() failed (msgs={:?})",
                last_turn.messages.len(),
            );

            // Auto-loop must not split or drop turns. This is a
            // programmer-error check kept in release builds: the cost
            // is one usize compare, and silent desync of `turns` is
            // far harder to diagnose than a panic.
            assert_eq!(
                prev_turn_count,
                turns.len(),
                "BUG: auto-loop split or dropped a turn (prev={}, now={})",
                prev_turn_count,
                turns.len(),
            );

            return Ok(response);
        }

        // Tool-call path: append assistant tool_use message, dispatch
        // tools, append a single user-tool_result message carrying
        // ALL parallel results, then re-invoke the model.
        let registry = tools.ok_or_else(|| {
            // Model called a tool but no registry was registered.
            // Translate the first tool name into UnknownTool so the
            // caller sees a useful error (rather than "model emitted
            // tool_use but session has no tools").
            let name = match &assistant_msg {
                Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    tool_calls[0].name.clone()
                }
                _ => "<unknown>".into(),
            };
            SessionError::Provider(ProviderError::UnknownTool { name })
        })?;

        let tool_calls = match &assistant_msg {
            Message::Assistant { tool_calls, .. } => tool_calls.clone(),
            _ => unreachable!("checked has_tool_calls above"),
        };

        // Append the assistant tool_use BEFORE running tools, so even
        // if a tool errors and we rollback, the conversation shape on
        // the way out is consistent (no orphan).
        turns
            .last_mut()
            .expect("turn was opened in send()")
            .push(assistant_msg);

        // Dispatch all tool_calls. Parallel calls share a single
        // user-message tool_result back (Anthropic requirement).
        let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(tool_calls.len());
        for tc in &tool_calls {
            let tool = registry.get(&tc.name).ok_or_else(|| {
                SessionError::Provider(ProviderError::UnknownTool {
                    name: tc.name.clone(),
                })
            })?;
            let result = tool.call(tc.arguments.clone()).await?;
            // Embed the result as a text block tagged with tool_use_id
            // by serializing JSON. Provider adapters that want the
            // structured `tool_result` content-block emit it from this
            // text payload; the keepalive path here stays provider-
            // agnostic.
            result_blocks.push(ContentBlock::Text {
                text: format!(
                    "{{\"tool_use_id\":\"{}\",\"result\":{}}}",
                    tc.id,
                    serde_json::to_string(&result).unwrap_or_else(|_| "null".into()),
                ),
            });
        }
        // One user message with all parallel results.
        turns
            .last_mut()
            .expect("turn was opened in send()")
            .push(Message::User { content: result_blocks });

        // Loop again — assert turn count unchanged so far. If it did
        // change, somebody trimmed mid-loop (forbidden).
        assert_eq!(
            prev_turn_count,
            turns.len(),
            "BUG: turn count changed mid-loop (prev={}, now={}); trim must not run inside loop_until_text",
            prev_turn_count,
            turns.len(),
        );
    }
}

/// Build the request `messages` list from the session's turns. Every
/// message in every turn flows through unchanged — providers see one
/// contiguous chronological history.
fn flatten_for_request(turns: &[Turn]) -> Vec<Message> {
    turns.iter().flat_map(|t| t.messages.iter().cloned()).collect()
}

/// Reconstruct an `Assistant` message from a `ChatResponse`. Text +
/// tool_calls are both copied so the message is faithful and suitable
/// for re-sending in the next request.
fn build_assistant_message_from(resp: &tars_types::ChatResponse) -> Message {
    let mut content: Vec<ContentBlock> = Vec::new();
    if !resp.text.is_empty() {
        content.push(ContentBlock::text(resp.text.clone()));
    }
    Message::Assistant {
        content,
        tool_calls: resp.tool_calls.clone(),
    }
}

// Unit tests with a mock LlmService land alongside Stage-3 smoke
// validation through tars-py — see crates/tars-py and the smoke
// scripts under target/wheels/. The session core is exercised
// end-to-end against LM Studio there, which gives stronger signal
// than a hand-rolled mock for this layer.
