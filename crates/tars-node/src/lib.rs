//! tars-node — Node.js bindings for tars.
//!
//! The TS-facing entry point is:
//!
//! ```ts
//! import { Pipeline } from '@leocaolab/tars-node';
//!
//! const pipeline = Pipeline.fromConfigPath('.arc/config.toml', 'gemini_pro');
//! const resp = await pipeline.complete({
//!     model: 'gemini-3.1-pro-preview',
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
//! Internals mirror `tars-py` one-for-one:
//!
//!   - **Process-wide tokio runtime** (`TOKIO` `OnceLock`). Each
//!     `complete()` runs on this single multi-thread runtime so a
//!     Node process never spawns one per call. napi-rs's `tokio_rt`
//!     feature dispatches our `async fn` onto it transparently.
//!   - **`Pipeline.from*` constructors** mirror tars-py
//!     (`from_config_path` / `from_str` / `from_default`). They run
//!     `ConfigManager::load_*` → `ProviderRegistry::from_config` →
//!     `Pipeline::default_chain` and store an `Arc<dyn LlmService>`.
//!   - **`complete(opts)`** maps the napi-friendly camelCase
//!     `CompleteOptions` → `tars_types::ChatRequest`, drives the
//!     held `Arc<dyn LlmService>`, drains the event stream into a
//!     `ChatResponse`, projects to `CompleteResult`.
//!   - **Errors** map `ConfigError` / `ProviderError` / inner async
//!     failures to `napi::Error`, which surfaces in JS as a rejected
//!     Promise.
//!
//! Out of scope for this milestone (M2): streaming AsyncIterator,
//! `run_task(...)` over the DAG executor, tool-calling
//! marshalling, per-call cancellation tokens, validator chains,
//! event-store wiring. Each is additive on top of this surface.

use std::sync::Arc;
use std::sync::OnceLock;

use futures::StreamExt;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::runtime::Runtime;

use tars_config::ConfigManager;
use tars_pipeline::{LlmService, Pipeline as RsPipeline, PipelineOpts};
use tars_provider::{
    LlmProvider, auth::basic, http_base::HttpProviderBase, registry::ProviderRegistry,
};
use tars_types::{
    Message, RequestContext,
    chat::{ChatRequest, ContentBlock},
    ids::ProviderId,
    model::{ModelHint, ThinkingMode},
    response::ChatResponseBuilder,
    schema::JsonSchema,
};

/// Process-wide tokio runtime shared by all `complete()` calls. Lazy
/// `OnceLock` so we pay the runtime-startup cost on the first call,
/// not at `require()` / `import` time — Node processes that load
/// `@leocaolab/tars-node` but never invoke a Pipeline stay light.
/// Multi-thread because providers may issue parallel HTTP requests
/// (batch APIs, fan-out routing in higher milestones).
static TOKIO: OnceLock<Runtime> = OnceLock::new();

fn tokio_rt() -> Result<&'static Runtime> {
    TOKIO.get_or_init(|| {
        Runtime::new().expect("tars-node: failed to build the shared tokio runtime")
    });
    // `get_or_init` returned `()` (we threw the runtime away on the
    // first race); `get` reads the actual handle. Both fast paths
    // hit `Some`.
    TOKIO
        .get()
        .ok_or_else(|| Error::from_reason("tars-node: tokio runtime not initialised"))
}

/// Smoke-test export — `import { hello } from '@leocaolab/tars-node'`
/// and `hello('world')` returns `'tars-node says hi, world'`. Pure
/// scaffolding sanity, kept for the Node-side smoke test.
#[napi]
pub fn hello(name: String) -> String {
    format!("tars-node says hi, {name}")
}

/// One failed bless assertion (Doc 28). `expected`/`actual` are JSON-encoded
/// strings so any value shape survives the FFI boundary.
#[napi(object)]
pub struct BlessDrift {
    pub selector: String,
    pub expected: String,
    pub actual: Option<String>,
    pub reason: String,
}

/// Result of [`blessCheck`]. `passed` is true when `drifts` is empty.
#[napi(object)]
pub struct BlessResult {
    pub passed: bool,
    pub drifts: Vec<BlessDrift>,
}

/// Load a committed bless file and check a completion's text against it
/// (Doc 28). `text` is decoded as JSON (chatty-tolerant), then each blessed
/// field is asserted. Mirrors `tars-py`'s `bless_check`.
#[napi]
pub fn bless_check(path: String, text: String) -> Result<BlessResult> {
    let value: serde_json::Value =
        tars_types::decode_json(&text, tars_types::StructuredOutputMode::None)
            .map_err(|e| Error::from_reason(format!("bless decode: {e}")))?;
    let bless = tars_types::Bless::load(std::path::Path::new(&path))
        .map_err(|e| Error::from_reason(format!("bless load: {e}")))?;
    let outcome = bless
        .check(&value)
        .map_err(|e| Error::from_reason(format!("bless check: {e}")))?;
    Ok(BlessResult {
        passed: outcome.is_pass(),
        drifts: outcome
            .drifts
            .iter()
            .map(|d| BlessDrift {
                selector: d.selector.clone(),
                expected: d.expected.to_string(),
                actual: d.actual.as_ref().map(|v| v.to_string()),
                reason: d.reason.clone(),
            })
            .collect(),
    })
}

// ── TS-facing shapes ─────────────────────────────────────────────────

/// Options for [`Pipeline::complete`]. Mirrors `tars-py`'s
/// `Pipeline.complete(**kwargs)` in camelCase.
#[napi(object)]
pub struct CompleteOptions {
    /// Model id (e.g. "gemini-3.1-pro-preview"). Required.
    pub model: String,
    /// User-turn text. Mutually exclusive with `messages`.
    pub user: Option<String>,
    /// System prompt.
    pub system: Option<String>,
    /// Pre-built message list as opaque JSON (`[{ role, content }, ...]`).
    /// Mutually exclusive with `user`. M3 will replace with a typed
    /// `MessageJs[]`; M2 keeps it `Value` so a Node caller with an
    /// existing OpenAI-shape message list can drop it in.
    pub messages: Option<serde_json::Value>,
    /// Cap on output tokens. Unset → provider default.
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature. Unset → provider default.
    pub temperature: Option<f64>,
    /// Enable thinking / extended reasoning (provider-specific).
    pub thinking: Option<bool>,
    /// JSON Schema the model MUST conform to. Triggers the provider's
    /// strict structured-output mode (Doc 01 §9).
    pub response_schema: Option<serde_json::Value>,
    /// When `response_schema` is set, require strict conformance
    /// (default true). False → schema is a hint, not enforced.
    pub response_schema_strict: Option<bool>,
    /// Free-form cohort tags surfaced in trajectory + event logs.
    pub tags: Option<Vec<String>>,
}

/// Result shape from [`Pipeline::complete`].
#[napi(object)]
pub struct CompleteResult {
    /// Aggregated assistant text.
    pub text: String,
    /// Token usage.
    pub usage: UsageJs,
    /// Model id the provider actually used (post-routing).
    pub model: Option<String>,
    /// Snake-case stop reason — "end_turn" / "max_tokens" /
    /// "content_filter" / "tool_use" / "other".
    pub stop_reason: Option<String>,
}

/// Mirrors `tars_types::Usage` — per-call token counters.
#[napi(object)]
pub struct UsageJs {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
    pub cache_creation_tokens: u32,
    pub thinking_tokens: u32,
}

/// Pipeline handle. One per provider per Node process is the
/// expected pattern (cheap to clone via `Arc` but expensive to build
/// — `from_config_path` reads + parses TOML and constructs the
/// middleware chain). Construct once at startup and hold the handle
/// for the process's lifetime.
#[napi]
pub struct Pipeline {
    /// Provider id this pipeline was built for. Surfaced via the
    /// `id` getter so callers can introspect which provider a
    /// handle wraps.
    id: String,
    /// The assembled middleware-stack-wrapped LlmService. Holds an
    /// `Arc<dyn LlmService>` so calls don't move it — `complete()`
    /// can `Arc::clone` and drive on the tokio runtime.
    inner: Arc<dyn LlmService>,
}

#[napi]
impl Pipeline {
    /// Construct from a `.arc/config.toml`-shaped file. `providerId`
    /// names which `[providers.<id>]` block in the TOML to bind to.
    /// Errors:
    ///   - file missing / unreadable / not valid TOML → ConfigError
    ///   - provider id absent from the config → ConfigError
    ///   - provider construction fails (e.g. bad API key shape) →
    ///     ProviderError
    #[napi(factory)]
    pub fn from_config_path(path: String, provider_id: String) -> Result<Pipeline> {
        let cfg = ConfigManager::load_from_file(&path)
            .map_err(|e| Error::from_reason(format!("load config {path:?}: {e}")))?;
        let provider = build_provider_from_cfg(&cfg, &provider_id)?;
        Ok(Self::from_provider(provider_id, provider))
    }

    /// Construct from inline TOML — same shape as a `.arc/config.toml`
    /// file. Useful for tests + programmatic configs without a tmpfile
    /// round-trip.
    #[napi(factory)]
    pub fn from_str(toml_text: String, provider_id: String) -> Result<Pipeline> {
        let cfg = ConfigManager::load_from_str(&toml_text)
            .map_err(|e| Error::from_reason(format!("parse inline TOML: {e}")))?;
        let provider = build_provider_from_cfg(&cfg, &provider_id)?;
        Ok(Self::from_provider(provider_id, provider))
    }

    /// Provider id this pipeline wraps.
    #[napi(getter)]
    pub fn id(&self) -> String {
        self.id.clone()
    }

    /// Single non-streaming chat-completion call.
    ///
    /// Builds a `ChatRequest` from `opts`, drives it through the
    /// held middleware stack, drains the resulting event stream,
    /// returns the aggregated text + usage + stop_reason. napi-rs
    /// marshals this as a `Promise<CompleteResult>` to JS callers.
    #[napi]
    pub async fn complete(&self, opts: CompleteOptions) -> Result<CompleteResult> {
        // Pull cohort tags before consuming opts into the request
        // builder — both downstream uses (RequestContext.tags and
        // build_request's ChatRequest construction) need ownership
        // bits out of `opts` and Rust's move semantics force one
        // copy-out here. Empty when caller didn't pass `tags`.
        let tags = opts.tags.clone().unwrap_or_default();
        let req = build_request(opts)?;
        let svc = Arc::clone(&self.inner);
        // napi-rs's `tokio_rt` feature already dispatched us onto our
        // shared runtime. The explicit `tokio_rt()?` call is the
        // membership check — surfaces a clean error if the runtime
        // hasn't initialised (shouldn't happen, but defensive).
        let _rt = tokio_rt()?;
        let ctx = RequestContext::test_default().with_tags(tags);
        let mut stream = svc
            .call(req, ctx)
            .await
            .map_err(|e| Error::from_reason(format!("provider call: {e}")))?;
        let mut builder = ChatResponseBuilder::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.map_err(|e| Error::from_reason(format!("stream event: {e}")))?;
            builder.apply(ev);
        }
        let resp = builder.finish();
        Ok(CompleteResult {
            text: resp.text,
            usage: UsageJs {
                input_tokens: resp.usage.input_tokens as u32,
                output_tokens: resp.usage.output_tokens as u32,
                cached_input_tokens: resp.usage.cached_input_tokens as u32,
                cache_creation_tokens: resp.usage.cache_creation_tokens as u32,
                thinking_tokens: resp.usage.thinking_tokens as u32,
            },
            model: Some(resp.actual_model),
            stop_reason: resp.stop_reason.map(|r| stop_reason_str(&r).to_string()),
        })
    }
}

impl Pipeline {
    /// Internal constructor shared by every `from_*` factory: wrap
    /// the resolved `Arc<dyn LlmProvider>` in TARS's default
    /// middleware onion (`Pipeline::default_chain`) so cache, retry,
    /// telemetry, and the rest are all active by construction.
    fn from_provider(id: String, provider: Arc<dyn LlmProvider>) -> Self {
        let opts = PipelineOpts::new(ProviderId::new(id.clone()));
        let pipeline = RsPipeline::default_chain(provider, opts);
        let inner: Arc<dyn LlmService> = Arc::new(pipeline);
        Self { id, inner }
    }
}

// ── Builder helpers ──────────────────────────────────────────────────

/// Shared body of `from_config_path` / `from_str`: resolve a provider
/// out of the loaded Config and surface a uniform napi error.
fn build_provider_from_cfg(
    cfg: &tars_config::Config,
    provider_id: &str,
) -> Result<Arc<dyn LlmProvider>> {
    let http = HttpProviderBase::default_arc()
        .map_err(|e| Error::from_reason(format!("build HTTP base: {e}")))?;
    let registry = ProviderRegistry::from_config(&cfg.providers, http, basic())
        .map_err(|e| Error::from_reason(format!("build provider registry: {e}")))?;
    let pid = ProviderId::new(provider_id.to_string());
    registry.get(&pid).ok_or_else(|| {
        let configured: Vec<String> = cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
        Error::from_reason(format!(
            "provider {provider_id:?} not in config. Configured: [{}]",
            configured.join(", "),
        ))
    })
}

/// Map a snake_case stop reason out of `tars_types::StopReason`.
/// Matches the wire form documented in CompleteResult.stop_reason
/// so JS callers can switch on it.
fn stop_reason_str(r: &tars_types::StopReason) -> &'static str {
    use tars_types::StopReason::*;
    match r {
        EndTurn => "end_turn",
        MaxTokens => "max_tokens",
        StopSequence => "stop_sequence",
        ToolUse => "tool_use",
        ContentFilter => "content_filter",
        Cancelled => "cancelled",
        // Forward-compat: tars_types::StopReason may grow new variants
        // in future releases. `_` keeps the binding non-exhaustively
        // matchable so an upstream addition doesn't break this crate
        // until we extend the table.
        _ => "other",
    }
}

/// Build a `ChatRequest` from the napi `CompleteOptions`. Mirrors
/// `tars-py::build_request` but for the napi shape.
fn build_request(opts: CompleteOptions) -> Result<ChatRequest> {
    if opts.user.is_some() && opts.messages.is_some() {
        return Err(Error::from_reason(
            "pass either `user` (single-turn) or `messages` (multi-turn), not both",
        ));
    }
    let mut req = ChatRequest {
        model: ModelHint::Explicit(opts.model),
        system: opts.system,
        messages: Vec::new(),
        tools: Vec::new(),
        tool_choice: Default::default(),
        structured_output: None,
        max_output_tokens: opts.max_output_tokens,
        temperature: opts.temperature.map(|t| t as f32),
        stop_sequences: Vec::new(),
        seed: None,
        cache_directives: Vec::new(),
        thinking: opts.thinking.map(map_thinking).unwrap_or_default(),
        enable_chat_template_thinking: opts.thinking,
    };
    if let Some(user) = opts.user {
        req.messages.push(Message::user_text(user));
    } else if let Some(msgs) = opts.messages {
        req.messages = parse_messages_json(msgs)?;
    } else {
        return Err(Error::from_reason(
            "must pass at least one of `user` or `messages`",
        ));
    }
    if let Some(schema) = opts.response_schema {
        req.structured_output = Some(JsonSchema {
            name: None,
            schema,
            strict: opts.response_schema_strict.unwrap_or(true),
        });
    }
    Ok(req)
}

/// Map the boolean `thinking` flag onto the typed
/// [`tars_types::ThinkingMode`]. `true` → Auto (provider picks the
/// depth); `false` → Off. Granular control (Budget(N) for specific
/// token caps) is M5+ work.
fn map_thinking(enabled: bool) -> ThinkingMode {
    if enabled {
        ThinkingMode::Auto
    } else {
        ThinkingMode::Off
    }
}

/// Parse the opaque `messages` JSON into typed `Message`s. Accepts
/// the canonical OpenAI-style `[{role: "user|assistant|system|tool",
/// content: "..."}, ...]` shape. M3 will switch to a typed
/// `MessageJs[]` napi struct; M2 keeps it permissive so existing
/// OpenAI-shape callers can drop in.
fn parse_messages_json(v: serde_json::Value) -> Result<Vec<Message>> {
    let arr = v
        .as_array()
        .ok_or_else(|| Error::from_reason("`messages` must be a JSON array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, m) in arr.iter().enumerate() {
        let role = m
            .get("role")
            .and_then(|r| r.as_str())
            .ok_or_else(|| Error::from_reason(format!("messages[{i}].role missing")))?;
        let content = m.get("content").and_then(|c| c.as_str()).ok_or_else(|| {
            Error::from_reason(format!("messages[{i}].content missing or not a string"))
        })?;
        let msg = match role {
            "user" => Message::user_text(content.to_string()),
            "assistant" => Message::assistant_text(content.to_string()),
            "system" => Message::System {
                content: vec![ContentBlock::text(content.to_string())],
            },
            other => {
                return Err(Error::from_reason(format!(
                    "messages[{i}].role unknown: {other:?} (expected user|assistant|system)"
                )));
            }
        };
        out.push(msg);
    }
    Ok(out)
}
