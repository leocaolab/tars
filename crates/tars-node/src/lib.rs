//! tars-node — Node.js bindings for tars.
//!
//! The TS-facing entry point is:
//!
//! ```ts
//! import { Pipeline } from '@leocaolab/tars-node';
//!
//! const pipeline = Pipeline.fromConfigPath('.arc/config.toml');
//! const resp = await pipeline.complete({
//!     model: 'claude-sonnet-4',
//!     user: 'Hello',
//!     system: 'You are a critic.',
//!     responseSchema: { type: 'object', /* … */ },
//!     responseSchemaStrict: true,
//!     maxOutputTokens: 3000,
//!     temperature: 0.0,
//! });
//!
//! console.log(resp.text);
//! console.log(resp.usage.inputTokens, resp.usage.outputTokens);
//! ```
//!
//! Internals mirror `tars-py`: one process-wide tokio runtime owned
//! by the binding, shared across all `complete()` calls (no
//! per-request runtime spawn). The binding releases the JS event
//! loop while the underlying async work runs by returning a
//! `Promise` that `napi-rs` resolves from a tokio task — Node's
//! caller stays unblocked exactly like Python's `py.allow_threads()`
//! pattern in `tars-py::complete()`.
//!
//! v0.1 surface — minimal:
//!
//!   - `Pipeline.fromConfigPath(path)` — construct from a
//!     `.arc/config.toml`-shaped file. (No env-var / inline-config
//!     path for v0.1; load-from-config is the only way in.)
//!   - `Pipeline.complete(opts)` — single non-streaming chat call.
//!     Returns `{ text, usage, ... }`.
//!
//! Out of scope for v0.1 (deferred to v0.2+):
//!
//!   - Streaming (`stream()` → AsyncIterator)
//!   - `run_task()` (the multi-step DAG executor) — requires
//!     marshalling Agent traits across the napi boundary, which
//!     needs more design.
//!   - Tools / tool_choice marshalling.
//!   - Per-call cancellation token.
//!
//! Those will land as additive features once v0.1 proves the
//! Pipeline.complete shape works end-to-end against a real
//! provider.

use napi::bindgen_prelude::*;
use napi_derive::napi;

/// Smoke-test export: `import { hello } from '@leocaolab/tars-node'`
/// and `hello('world')` returns `'tars-node says hi, world'`. Pure
/// scaffolding sanity — no LlmService touched. Will be removed once
/// `Pipeline.complete()` is wired and a real smoke test replaces
/// this one.
#[napi]
pub fn hello(name: String) -> String {
    format!("tars-node says hi, {name}")
}

/// TS-facing shape for chat-completion options. Mirrors the
/// `Pipeline.complete()` kwargs in `tars-py::lib.rs::Pipeline::complete`,
/// with camelCase field names for idiomatic JS.
///
/// Why a struct (not positional args)? TS callers see one
/// destructurable options object; napi-rs auto-generates a
/// `CompleteOptions` interface in the `.d.ts` so the call site has
/// strict-typed IDE completion.
#[napi(object)]
pub struct CompleteOptions {
    /// Model id (e.g. "claude-sonnet-4", "gpt-4o"). Required.
    pub model: String,
    /// User-turn text. Either `user` OR `messages` MUST be set.
    pub user: Option<String>,
    /// System prompt.
    pub system: Option<String>,
    /// Pre-built message list — `[{ role, content }, ...]`. Use
    /// instead of `user` for multi-turn / structured conversations.
    /// (v0.1 takes them as opaque JSON; structured types land in
    /// v0.2.)
    pub messages: Option<serde_json::Value>,
    /// Cap on output tokens. Unset = provider default.
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature. Unset = provider default.
    pub temperature: Option<f64>,
    /// Enable thinking / chain-of-thought (provider-specific).
    pub thinking: Option<bool>,
    /// JSON Schema the model MUST conform its output to. When set,
    /// the provider's strict structured-output mode is used (refer
    /// to Doc 01 §9 — OpenAI `response_format=json_schema`, Gemini
    /// `responseSchema`, Anthropic forced-tool emulation).
    pub response_schema: Option<serde_json::Value>,
    /// When `response_schema` is set, whether to require strict
    /// conformance (default true).
    pub response_schema_strict: Option<bool>,
    /// Free-form tags for trajectory/cohort labelling. Same shape
    /// as `tars-py`'s `tags=[...]` kwarg.
    pub tags: Option<Vec<String>>,
}

/// TS-facing shape for a chat-completion result. The 1:1 mirror of
/// `Response` in `tars-py::lib.rs`, minus the streaming-only fields.
#[napi(object)]
pub struct CompleteResult {
    /// Aggregated assistant text. Empty string if the model returned
    /// no text content (e.g. content-filtered, or a tool-call-only
    /// response).
    pub text: String,
    /// Token usage. Same shape as tars-types::Usage.
    pub usage: UsageJs,
    /// Model id the provider actually used (may differ from the
    /// requested one when an alias was resolved).
    pub model: Option<String>,
    /// Why the model stopped — "end_turn" / "max_tokens" /
    /// "content_filter" / "tool_use" / "other".
    pub stop_reason: Option<String>,
}

/// Mirrors `tars_types::Usage` — token counters per call.
#[napi(object)]
pub struct UsageJs {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
    pub cache_creation_tokens: u32,
    pub thinking_tokens: u32,
}

/// Pipeline handle. One per TARS-aware Node process is the intended
/// pattern (the underlying [`tars_pipeline::Pipeline`] holds the
/// provider registry + middleware chain — cheap to clone via Arc but
/// expensive to build), so call sites should construct once at
/// startup and hold the reference for the process's lifetime.
///
/// v0.1: this is a stub. `from_config_path` returns Err("not yet
/// implemented") and `complete` likewise. The wiring lands in the
/// follow-up commit once napi build is confirmed green end-to-end.
#[napi]
pub struct Pipeline {
    // TODO(v0.1.1): hold an `Arc<dyn LlmService>` here once the
    // config-load path is wired (`tars_config::ConfigManager` →
    // `ProviderRegistry::from_config` → `tars_pipeline::Pipeline`).
    // Placeholder so the napi class isn't completely empty.
    config_path: String,
}

#[napi]
impl Pipeline {
    /// Construct from a `.arc/config.toml`-shaped file. v0.1 stub —
    /// the real impl will load + validate the config, build the
    /// provider registry, and assemble the middleware chain.
    #[napi(factory)]
    pub fn from_config_path(path: String) -> Result<Pipeline> {
        // v0.1 just stores the path; v0.1.1 wires the real loader.
        Ok(Pipeline { config_path: path })
    }

    /// Read-only accessor for tests / diagnostics.
    #[napi(getter)]
    pub fn config_path(&self) -> String {
        self.config_path.clone()
    }

    /// Single non-streaming chat-completion call. v0.1 stub —
    /// returns a synthetic response so the napi-rs build pipeline
    /// can be validated end-to-end (compile → bind → JS import →
    /// await → result-shape check) before any real provider call
    /// is wired through.
    ///
    /// v0.1.1 will:
    ///   - build a `ChatRequest` from `opts`
    ///   - drive it through the held `Arc<dyn LlmService>`
    ///   - drain the event stream into a `ChatResponse`
    ///   - project that into `CompleteResult`
    ///
    /// Marked `async` so napi-rs returns a JS Promise that resolves
    /// on the tokio runtime — same pattern Node clients expect of
    /// any async-IO-bound binding.
    #[napi]
    pub async fn complete(&self, opts: CompleteOptions) -> Result<CompleteResult> {
        // v0.1 stub: echo back enough of the request shape that a
        // smoke test can verify the napi marshalling round-trip
        // worked. NOT a real LLM call — the placeholder text makes
        // that obvious to any caller that mistakes v0.1 for v0.1.1.
        let echoed = opts
            .user
            .as_deref()
            .or(opts.system.as_deref())
            .unwrap_or("(no user / system text)")
            .to_string();
        Ok(CompleteResult {
            text: format!(
                "[tars-node v0.1 stub] would call model={} with: {}",
                opts.model, echoed,
            ),
            usage: UsageJs {
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_tokens: 0,
                thinking_tokens: 0,
            },
            model: Some(opts.model),
            stop_reason: Some("end_turn".into()),
        })
    }
}
