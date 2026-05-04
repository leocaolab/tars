//! Python bindings for `tars` — three planned layers, this commit
//! ships layers 1 + 2:
//!
//! - **Layer 1: `Provider`** — raw backend, no middleware. Useful when
//!   the user wants their own retry / cache / circuit-breaker policy
//!   in Python and just wants TARS for the unified provider abstraction.
//! - **Layer 2: `Pipeline`** — middleware-wrapped service. Same
//!   `complete()` surface, but cache + retry + telemetry are engaged
//!   automatically. This is the common case.
//! - **Layer 3 (deferred)**: agent runtime — `run_task`, Trajectory,
//!   ToolRegistry. Bigger surface; lands in a follow-on commit.
//!
//! ## Design notes
//!
//! Both `Provider` and `Pipeline` hold an `Arc<dyn LlmService>` —
//! they only differ in construction. Sharing the call-site machinery
//! lets the Python API stay symmetric: a user can swap `Provider` for
//! `Pipeline` (or vice versa) without changing call sites.
//!
//! Async is bridged via a process-wide multi-threaded tokio runtime
//! (`TOKIO`). `complete()` releases the GIL via `py.allow_threads`
//! before blocking on the runtime so other Python threads keep
//! working during the LLM round-trip — important for any non-trivial
//! Python application that fans out concurrent calls.
//!
//! ## Build + verify
//!
//! ```bash
//! cd crates/tars-py
//! maturin develop --release
//! python -c "import tars; print(tars.version())"
//! ```

use std::sync::{Arc, LazyLock};

use futures::StreamExt;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyList};

use tars_cache::CacheKeyFactory;
use tars_config::ConfigManager;
use tars_pipeline::{
    CacheLookupMiddleware, LlmService, Pipeline as RsPipeline, ProviderService, RetryMiddleware,
    TelemetryMiddleware,
};
use tars_provider::{
    auth::basic, http_base::HttpProviderBase, registry::ProviderRegistry, LlmProvider,
};
use tars_types::{
    ChatRequest, ChatResponseBuilder, ContentBlock, Message, ModelHint, ProviderId,
    RequestContext, StopReason,
};

/// Process-wide tokio runtime. Single instance amortizes the
/// thread-pool cost across all `Provider` / `Pipeline` calls.
/// Multi-threaded so async I/O concurrency works inside one
/// `complete()` invocation (provider adapters spawn background reads).
static TOKIO: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime init")
});

// ── Result + Usage ────────────────────────────────────────────────────

/// Token usage breakdown returned alongside every [`Response`].
/// Mirrors `tars_types::Usage`'s shape but exposed as a `#[pyclass]`
/// so Python sees a regular object with attribute access.
#[pyclass(frozen, get_all)]
#[derive(Clone, Debug)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_tokens: u64,
    thinking_tokens: u64,
}

#[pymethods]
impl Usage {
    fn __repr__(&self) -> String {
        format!(
            "Usage(input={}, output={}, cached_input={}, cache_creation={}, thinking={})",
            self.input_tokens,
            self.output_tokens,
            self.cached_input_tokens,
            self.cache_creation_tokens,
            self.thinking_tokens,
        )
    }
    fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.thinking_tokens
    }
}

/// One completed LLM call's response. Always has `.text` (possibly
/// empty), `.usage`, and `.stop_reason`. `.thinking` is only set for
/// reasoning-capable models that emitted reasoning tokens via the
/// OpenAI `reasoning_content` channel (o1 / DeepSeek-R1 /
/// Qwen3-thinking / etc.).
#[pyclass(frozen, get_all)]
#[derive(Clone, Debug)]
struct Response {
    text: String,
    thinking: String,
    usage: Usage,
    /// `"end_turn" / "max_tokens" / "stop_sequence" / "tool_use" / ...`
    /// — string form of `tars_types::StopReason`.
    stop_reason: String,
}

#[pymethods]
impl Response {
    fn __repr__(&self) -> String {
        format!(
            "Response(text={:?}, stop_reason={:?}, usage={})",
            truncate_for_repr(&self.text, 60),
            self.stop_reason,
            self.usage.__repr__(),
        )
    }
}

fn truncate_for_repr(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

// ── Layer 1: Provider (raw backend, no middleware) ────────────────────

/// Raw provider backend. Bypasses all TARS middleware (cache / retry
/// / breaker / routing). Use when you want to manage those yourself
/// in Python, OR when you want to measure baseline provider behaviour
/// without our middleware in the loop.
///
/// For the common case where you want middleware engaged, use
/// [`Pipeline`] instead.
#[pyclass(frozen)]
struct Provider {
    id: String,
    inner: Arc<dyn LlmService>,
    capabilities_summary: CapabilitiesSummary,
}

#[derive(Clone)]
struct CapabilitiesSummary {
    max_context_tokens: u64,
    max_output_tokens: u64,
    supports_tool_use: bool,
    supports_vision: bool,
    supports_thinking: bool,
    streaming: bool,
}

impl CapabilitiesSummary {
    fn from(caps: &tars_types::Capabilities) -> Self {
        Self {
            // Capabilities uses u32 for token limits; widen to u64 for
            // Python-friendly large-number arithmetic (Python ints are
            // arbitrary-precision but other Usage fields are u64, so
            // staying consistent avoids type-juggling on the Python side).
            max_context_tokens: u64::from(caps.max_context_tokens),
            max_output_tokens: u64::from(caps.max_output_tokens),
            supports_tool_use: caps.supports_tool_use,
            supports_vision: caps.supports_vision,
            supports_thinking: caps.supports_thinking,
            streaming: caps.streaming,
        }
    }

    fn into_dict<'py>(self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("max_context_tokens", self.max_context_tokens)?;
        d.set_item("max_output_tokens", self.max_output_tokens)?;
        d.set_item("supports_tool_use", self.supports_tool_use)?;
        d.set_item("supports_vision", self.supports_vision)?;
        d.set_item("supports_thinking", self.supports_thinking)?;
        d.set_item("streaming", self.streaming)?;
        Ok(d)
    }
}

#[pymethods]
impl Provider {
    /// Construct a Provider from a TARS TOML config file.
    /// `provider_id` selects which entry under `[providers.X]` to use.
    #[staticmethod]
    fn from_config(path: String, provider_id: String) -> PyResult<Self> {
        let provider = build_provider(&path, &provider_id)?;
        let caps = CapabilitiesSummary::from(provider.capabilities());
        let inner: Arc<dyn LlmService> = ProviderService::new(provider);
        Ok(Self { id: provider_id, inner, capabilities_summary: caps })
    }

    #[getter]
    fn id(&self) -> &str {
        &self.id
    }

    /// Capability summary as a dict (max tokens, supports flags).
    #[getter]
    fn capabilities<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        self.capabilities_summary.clone().into_dict(py)
    }

    fn __repr__(&self) -> String {
        format!("Provider(id={:?}, layer=raw)", self.id)
    }

    /// Run one completion. Blocks the calling thread on the underlying
    /// async stream — releases the GIL during the wait.
    #[pyo3(signature = (
        model,
        *,
        user = None,
        system = None,
        messages = None,
        max_output_tokens = None,
        temperature = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn complete(
        &self,
        py: Python<'_>,
        model: String,
        user: Option<String>,
        system: Option<String>,
        messages: Option<Bound<'_, PyList>>,
        max_output_tokens: Option<u32>,
        temperature: Option<f32>,
    ) -> PyResult<Response> {
        let req = build_request(model, user, system, messages, max_output_tokens, temperature)?;
        run_complete(py, self.inner.clone(), req)
    }
}

// ── Layer 2: Pipeline (middleware-wrapped) ────────────────────────────

/// Pipeline-wrapped service. Same `complete()` surface as [`Provider`]
/// but cache + retry + telemetry are engaged automatically. This is
/// the common case for production use — Python doesn't have to
/// re-implement any of those.
#[pyclass(frozen)]
struct Pipeline {
    id: String,
    inner: Arc<dyn LlmService>,
    capabilities_summary: CapabilitiesSummary,
    layer_names: Vec<String>,
}

#[pymethods]
impl Pipeline {
    /// Construct a Pipeline from a TARS TOML config file.
    /// Wraps the named provider with TARS's default middleware stack
    /// (Telemetry → CacheLookup → Retry → Provider).
    #[staticmethod]
    fn from_config(path: String, provider_id: String) -> PyResult<Self> {
        let provider = build_provider(&path, &provider_id)?;
        let caps = CapabilitiesSummary::from(provider.capabilities());
        let cache_origin = ProviderId::new(provider_id.clone());

        // Default cache: in-memory L1 only — Python users pinning a
        // path can layer SQLite via a future `Pipeline.builder()`.
        let cache_registry = tars_cache::MemoryCacheRegistry::default_arc();
        let cache_factory = CacheKeyFactory::new(1);

        let pipeline = RsPipeline::builder(provider)
            .layer(TelemetryMiddleware::new())
            .layer(CacheLookupMiddleware::new(
                cache_registry,
                cache_factory,
                cache_origin,
            ))
            .layer(RetryMiddleware::default())
            .build();

        let layer_names: Vec<String> =
            pipeline.layer_names().iter().map(|s| s.to_string()).collect();
        let inner: Arc<dyn LlmService> = Arc::new(pipeline);
        Ok(Self {
            id: provider_id,
            inner,
            capabilities_summary: caps,
            layer_names,
        })
    }

    #[getter]
    fn id(&self) -> &str {
        &self.id
    }

    #[getter]
    fn capabilities<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        self.capabilities_summary.clone().into_dict(py)
    }

    /// Outer-to-inner middleware names. E.g.
    /// `["telemetry", "cache_lookup", "retry"]` means a request hits
    /// telemetry first.
    #[getter]
    fn layer_names(&self) -> Vec<String> {
        self.layer_names.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "Pipeline(id={:?}, layers={:?})",
            self.id, self.layer_names,
        )
    }

    #[pyo3(signature = (
        model,
        *,
        user = None,
        system = None,
        messages = None,
        max_output_tokens = None,
        temperature = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn complete(
        &self,
        py: Python<'_>,
        model: String,
        user: Option<String>,
        system: Option<String>,
        messages: Option<Bound<'_, PyList>>,
        max_output_tokens: Option<u32>,
        temperature: Option<f32>,
    ) -> PyResult<Response> {
        let req = build_request(model, user, system, messages, max_output_tokens, temperature)?;
        run_complete(py, self.inner.clone(), req)
    }
}

// ── Shared helpers ────────────────────────────────────────────────────

/// Common: load config, build registry, return the named provider.
/// Used by both `Provider::from_config` and `Pipeline::from_config`.
fn build_provider(path: &str, provider_id: &str) -> PyResult<Arc<dyn LlmProvider>> {
    let cfg = ConfigManager::load_from_file(path).map_err(|e| {
        PyRuntimeError::new_err(format!("loading config from {path:?}: {e}"))
    })?;
    let http = HttpProviderBase::default_arc()
        .map_err(|e| PyRuntimeError::new_err(format!("building HTTP base: {e}")))?;
    let registry = ProviderRegistry::from_config(&cfg.providers, http, basic())
        .map_err(|e| PyRuntimeError::new_err(format!("building provider registry: {e}")))?;
    let pid = ProviderId::new(provider_id.to_string());
    registry.get(&pid).ok_or_else(|| {
        let configured: Vec<String> =
            cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
        PyValueError::new_err(format!(
            "provider {provider_id:?} not in config. Configured: [{}]",
            configured.join(", "),
        ))
    })
}

/// Build a [`ChatRequest`] from Python kwargs. Accepts either:
/// - `user="..."` for a single-turn user message, OR
/// - `messages=[{role, content}, ...]` for multi-turn,
/// - `system="..."` always optional (top-level system prompt).
fn build_request(
    model: String,
    user: Option<String>,
    system: Option<String>,
    messages: Option<Bound<'_, PyList>>,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
) -> PyResult<ChatRequest> {
    if user.is_some() && messages.is_some() {
        return Err(PyValueError::new_err(
            "pass either `user=` (single-turn) or `messages=` (multi-turn), not both",
        ));
    }
    let msgs: Vec<Message> = if let Some(u) = user {
        vec![Message::user_text(u)]
    } else if let Some(list) = messages {
        list.iter()
            .map(|item| message_from_py(&item))
            .collect::<PyResult<Vec<_>>>()?
    } else {
        return Err(PyValueError::new_err(
            "must pass `user=` or `messages=`",
        ));
    };
    Ok(ChatRequest {
        model: ModelHint::Explicit(model),
        system,
        messages: msgs,
        tools: Vec::new(),
        tool_choice: Default::default(),
        structured_output: None,
        max_output_tokens,
        temperature,
        stop_sequences: Vec::new(),
        seed: None,
        cache_directives: Vec::new(),
        thinking: Default::default(),
    })
}

/// Convert one Python `{role, content}` dict to a `Message`. Content
/// must be a string in this v1 (multimodal blocks come later).
fn message_from_py(item: &Bound<'_, PyAny>) -> PyResult<Message> {
    let dict = item.downcast::<PyDict>().map_err(|_| {
        PyValueError::new_err("each message must be a dict with `role` and `content` keys")
    })?;
    let role = dict
        .get_item("role")?
        .ok_or_else(|| PyValueError::new_err("message missing `role`"))?
        .extract::<String>()?;
    let content_obj = dict
        .get_item("content")?
        .ok_or_else(|| PyValueError::new_err("message missing `content`"))?;
    let content_str: String = content_obj.extract().map_err(|_| {
        PyValueError::new_err(
            "message `content` must be a string (multimodal blocks not supported in v1)",
        )
    })?;
    let blocks = vec![ContentBlock::text(content_str)];
    match role.as_str() {
        "user" => Ok(Message::User { content: blocks }),
        "assistant" => Ok(Message::Assistant { content: blocks, tool_calls: Vec::new() }),
        "system" => Ok(Message::System { content: blocks }),
        "tool" => Err(PyValueError::new_err(
            "tool-result messages need a tool_call_id; not supported via this convenience surface yet",
        )),
        other => Err(PyValueError::new_err(format!("unknown role: {other}"))),
    }
}

/// Drain the LLM stream into a [`Response`]. Releases the GIL while
/// waiting on the async runtime so other Python threads keep working.
fn run_complete(
    py: Python<'_>,
    svc: Arc<dyn LlmService>,
    req: ChatRequest,
) -> PyResult<Response> {
    py.allow_threads(|| {
        TOKIO.block_on(async move {
            let mut stream = svc.call(req, RequestContext::test_default()).await.map_err(|e| {
                PyRuntimeError::new_err(format!("provider call failed: {e}"))
            })?;
            let mut builder = ChatResponseBuilder::new();
            while let Some(ev) = stream.next().await {
                let ev = ev.map_err(|e| {
                    PyRuntimeError::new_err(format!("stream error mid-call: {e}"))
                })?;
                builder.apply(ev);
            }
            let resp = builder.finish();
            Ok(Response {
                text: resp.text,
                thinking: resp.thinking,
                usage: Usage {
                    input_tokens: resp.usage.input_tokens,
                    output_tokens: resp.usage.output_tokens,
                    cached_input_tokens: resp.usage.cached_input_tokens,
                    cache_creation_tokens: resp.usage.cache_creation_tokens,
                    thinking_tokens: resp.usage.thinking_tokens,
                },
                stop_reason: resp
                    .stop_reason
                    .map(|r| stop_reason_str(&r).to_string())
                    .unwrap_or_else(|| "none".to_string()),
            })
        })
    })
}

fn stop_reason_str(r: &StopReason) -> &'static str {
    match r {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::ContentFilter => "content_filter",
        StopReason::Cancelled => "cancelled",
        StopReason::Other => "other",
    }
}

// ── Module ────────────────────────────────────────────────────────────

#[pyfunction]
fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// PyO3 module entry point. Symbol must be `_tars_py` to match
/// `pyproject.toml`'s `module-name = "tars._tars_py"`. Public Python
/// surface is curated by `python/tars/__init__.py`.
#[pymodule]
fn _tars_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_class::<Provider>()?;
    m.add_class::<Pipeline>()?;
    m.add_class::<Response>()?;
    m.add_class::<Usage>()?;
    Ok(())
}
