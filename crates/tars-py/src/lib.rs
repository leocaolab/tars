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

mod context;
mod errors;
mod eval;
mod handle;
mod session;
mod validation;

use std::sync::{Arc, LazyLock};

use futures::StreamExt;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyList};

use tars_config::{Config, ConfigManager, default_config_path};

use crate::errors::{config_to_py, provider_to_py, runtime_to_py};
use tars_pipeline::{
    CostPolicy, EnsembleService, LatencyMetric, LatencyPolicy, LatencyStatsRegistry, LlmService,
    Pipeline as RsPipeline, ProviderService, RoutingService, StaticPolicy,
};
use tars_provider::{LlmProvider, registry::ProviderRegistry};
use tars_types::{
    ChatRequest, ChatResponseBuilder, ContentBlock, Message, ModelHint, ProviderId, RUN_CONTEXT,
    RequestContext, StopReason,
};

/// Process-wide tokio runtime. Single instance amortizes the
/// thread-pool cost across all `Provider` / `Pipeline` calls.
/// Multi-threaded so async I/O concurrency works inside one
/// `complete()` invocation (provider adapters spawn background reads).
pub(crate) static TOKIO: LazyLock<std::io::Result<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
});

/// Access the process-wide runtime, surfacing a build failure as a
/// Python `RuntimeError` instead of panicking the interpreter.
///
/// Runtime construction only fails under OS resource exhaustion (thread
/// / fd limits), but that failure used to `expect()`-panic inside a
/// `LazyLock` accessed by every call — tearing down the work rather
/// than handing Python a catchable error. The `LazyLock` caches the
/// outcome, so a doomed build isn't retried per call.
pub(crate) fn tokio_runtime() -> PyResult<&'static tokio::runtime::Runtime> {
    TOKIO.as_ref().map_err(|e| {
        pyo3::exceptions::PyRuntimeError::new_err(format!(
            "failed to initialize tars async runtime: {e}"
        ))
    })
}

// ── Result + Usage ────────────────────────────────────────────────────

/// Token usage breakdown returned alongside every [`Response`].
/// Mirrors `tars_types::Usage`'s shape but exposed as a `#[pyclass]`
/// so Python sees a regular object with attribute access.
#[pyclass(frozen, get_all)]
#[derive(Clone, Debug)]
pub(crate) struct Usage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) thinking_tokens: u64,
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
/// empty), `.usage`, `.stop_reason`, `.telemetry`, and
/// `.validation_summary`. `.thinking` is only set for reasoning-capable
/// models that emitted reasoning tokens via the OpenAI
/// `reasoning_content` channel (o1 / DeepSeek-R1 / Qwen3-thinking / etc.).
#[pyclass(frozen, get_all)]
#[derive(Clone, Debug)]
pub(crate) struct Response {
    pub(crate) text: String,
    pub(crate) thinking: String,
    pub(crate) usage: Usage,
    /// `"end_turn" / "max_tokens" / "stop_sequence" / "tool_use" / ...`
    /// — string form of `tars_types::StopReason`.
    pub(crate) stop_reason: String,
    /// Per-call observability data: cache hit, retry attempts,
    /// per-layer latency, layer trace. See `Telemetry` class.
    pub(crate) telemetry: Telemetry,
    /// Per-validator outcomes for this call. Empty when no validators
    /// ran (caller didn't pass `validators=` or empty list). See
    /// `ValidationSummary` class.
    pub(crate) validation_summary: ValidationSummary,
}

#[pymethods]
impl Response {
    fn __repr__(&self) -> String {
        format!(
            "Response(text={:?}, stop_reason={:?}, usage={}, telemetry={}, validation_summary={})",
            truncate_for_repr(&self.text, 60),
            self.stop_reason,
            self.usage.__repr__(),
            self.telemetry.__repr__(),
            self.validation_summary.__repr__(),
        )
    }
}

/// Aggregated record of all validators that ran during one
/// `Pipeline.complete` call. Populated by `ValidationMiddleware`;
/// stays empty when the Pipeline has no validators attached.
///
/// Fields:
///
/// - `validators_run: list[str]` — names of validators that ran, in
///   registration order. Captures the chain shape (BTreeMap-keyed
///   `outcomes` doesn't preserve order).
/// - `outcomes: dict[str, dict]` — keyed by validator name. Each
///   value is one of:
///     - `{"outcome": "pass"}`
///     - `{"outcome": "filter", "dropped": [...]}`
///     - `{"outcome": "annotate", "metrics": {...}}`
///   Reject doesn't appear here — Reject short-circuits into a
///   `TarsProviderError(kind="validation_failed")` and there's no
///   Response object to attach a summary to.
/// - `total_wall_ms: int` — wall time spent in ValidationMiddleware
///   for this call.
#[pyclass(frozen, name = "ValidationSummary")]
#[derive(Clone, Debug, Default)]
pub(crate) struct ValidationSummary {
    #[pyo3(get)]
    pub(crate) validators_run: Vec<String>,
    /// Stored as `serde_json::Value` (Python-convertibility on
    /// `serde_json::Value` doesn't exist). Exposed via the `outcomes`
    /// getter, which converts on demand.
    pub(crate) outcomes_json: serde_json::Value,
    #[pyo3(get)]
    pub(crate) total_wall_ms: u64,
}

#[pymethods]
impl ValidationSummary {
    #[getter]
    fn outcomes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let json_mod = py.import("json")?;
        // Propagate a serialization failure rather than silently handing
        // Python an empty `{}` — a `{}` would masquerade as "no outcomes"
        // and hide the real failure from the caller.
        let s = serde_json::to_string(&self.outcomes_json).map_err(|e| {
            pyo3::exceptions::PyRuntimeError::new_err(format!(
                "failed to serialize validation outcomes: {e}"
            ))
        })?;
        let obj = json_mod.call_method1("loads", (s,))?;
        obj.downcast_into::<pyo3::types::PyDict>()
            .map_err(PyErr::from)
    }

    fn __repr__(&self) -> String {
        format!(
            "ValidationSummary(validators_run={:?}, total_wall_ms={})",
            self.validators_run, self.total_wall_ms
        )
    }
}

/// Convert a typed `tars_types::ValidationSummary` into the
/// Python-facing pyclass shape.
pub(crate) fn validation_summary_to_py(s: tars_types::ValidationSummary) -> ValidationSummary {
    let mut outcomes = serde_json::Map::new();
    for (name, oc) in s.outcomes {
        let val = match oc {
            tars_types::OutcomeSummary::Pass => serde_json::json!({"outcome": "pass"}),
            tars_types::OutcomeSummary::Filter { dropped } => {
                serde_json::json!({"outcome": "filter", "dropped": dropped})
            }
            tars_types::OutcomeSummary::Annotate { metrics } => {
                serde_json::json!({"outcome": "annotate", "metrics": metrics})
            }
            // `#[non_exhaustive]` — surface unknown variant names to
            // the caller rather than panic. Log it and preserve the
            // Debug rendering so a tars-py older than tars-types doesn't
            // discard the variant's structured data entirely.
            ref other => {
                tracing::warn!(
                    validator = %name,
                    outcome = ?other,
                    "validation_summary: unknown OutcomeSummary variant mapped to \
                     `unknown` (tars-py is older than tars-types — upgrade to surface it)",
                );
                serde_json::json!({"outcome": "unknown", "debug": format!("{other:?}")})
            }
        };
        outcomes.insert(name, val);
    }
    ValidationSummary {
        validators_run: s.validators_run,
        outcomes_json: serde_json::Value::Object(outcomes),
        total_wall_ms: s.total_wall_ms,
    }
}

/// Per-call observability data filled in by the middleware stack.
///
/// Field semantics match `tars_types::TelemetryAccumulator`:
///
/// - `cache_hit`: middleware-cache (in-mem L1 / disk L2) hit. Distinct
///   from `usage.cached_input_tokens` — that's the *provider's* prompt
///   cache.
/// - `retry_count`: how many retries happened. 0 = first attempt
///   succeeded.
/// - `retry_attempts`: list of dicts `{kind, retry_after_ms}` — one per
///   failed attempt that was retried.
/// - `provider_latency_ms`: total provider HTTP+SSE wall time across
///   all retry attempts. None for paths that didn't go through a
///   provider (e.g. cache hits short-circuit).
/// - `pipeline_total_ms`: end-to-end wall time including middleware
///   overhead and stream drain.
/// - `layers`: middleware names that participated, outermost-first.
#[pyclass(frozen, get_all)]
#[derive(Clone, Debug, Default)]
pub(crate) struct Telemetry {
    pub(crate) cache_hit: bool,
    pub(crate) retry_count: u32,
    pub(crate) retry_attempts: Vec<RetryAttemptPy>,
    pub(crate) provider_latency_ms: Option<u64>,
    pub(crate) pipeline_total_ms: Option<u64>,
    pub(crate) layers: Vec<String>,
}

#[pymethods]
impl Telemetry {
    fn __repr__(&self) -> String {
        format!(
            "Telemetry(cache_hit={}, retry_count={}, provider_ms={:?}, total_ms={:?}, layers={:?})",
            self.cache_hit,
            self.retry_count,
            self.provider_latency_ms,
            self.pipeline_total_ms,
            self.layers,
        )
    }
}

/// One retry attempt record. `kind` is sourced from the typed
/// [`ProviderErrorKind`] discriminator on the underlying
/// [`tars_types::ProviderError`]; it crosses into Python as a
/// snake-case string (`"rate_limited"`, `"network"`, …) matching the
/// stable wire form of `TarsProviderError.kind`. `retry_after_ms` is
/// the actual backoff slept.
#[pyclass(frozen, name = "RetryAttempt")]
#[derive(Clone, Debug)]
pub(crate) struct RetryAttemptPy {
    /// Internally typed (`ProviderErrorKind`); not exposed directly to
    /// Python (`get_all` removed). The `#[getter] kind()` below
    /// projects to the snake-case `&str` Python consumers already
    /// see, so the external contract is unchanged.
    pub(crate) kind: tars_types::ProviderErrorKind,
    #[pyo3(get)]
    pub(crate) retry_after_ms: Option<u64>,
}

#[pymethods]
impl RetryAttemptPy {
    /// Snake-case kind tag — same string the field exposed when it
    /// was `String`. Backed by the typed [`ProviderErrorKind`] now,
    /// so producers can't accidentally write `"rate_limmited"`.
    #[getter]
    fn kind(&self) -> &'static str {
        self.kind.as_str()
    }

    fn __repr__(&self) -> String {
        format!(
            "RetryAttempt(kind={:?}, retry_after_ms={:?})",
            self.kind.as_str(),
            self.retry_after_ms
        )
    }
}

/// Shared repr truncation so submodules (e.g. `validation`) format
/// truncated text the same way the top-level reprs do.
pub(crate) fn truncate_for_repr(s: &str, max: usize) -> String {
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

impl Provider {
    /// Internal: wrap a built provider as a layer-1 `Provider` Python
    /// object. Shared between `from_config` and `from_str`.
    fn from_provider(id: String, provider: Arc<dyn LlmProvider>) -> Self {
        let capabilities_summary = CapabilitiesSummary::from(provider.capabilities());
        let inner: Arc<dyn LlmService> = ProviderService::new(provider);
        Self {
            id,
            inner,
            capabilities_summary,
        }
    }
}

#[pymethods]
impl Provider {
    /// Construct a Provider from a TARS TOML config file.
    /// `provider_id` selects which entry under `[providers.X]` to use.
    #[staticmethod]
    fn from_config(path: String, provider_id: String) -> PyResult<Self> {
        let provider = build_provider(&path, &provider_id)?;
        Ok(Self::from_provider(provider_id, provider))
    }

    /// Construct a Provider from inline TOML text. Equivalent to
    /// `from_config` but skips the round-trip through a tmpfile —
    /// useful for tests and programmatic construction.
    #[staticmethod]
    fn from_str(toml_text: &str, provider_id: String) -> PyResult<Self> {
        let provider = build_provider_from_str(toml_text, &provider_id)?;
        Ok(Self::from_provider(provider_id, provider))
    }

    /// Construct a Provider from the default user-level config at
    /// `~/.tars/config.toml`. Raises `TarsConfigError` if the file is
    /// missing — run `tars init` to bootstrap one.
    #[staticmethod]
    fn from_default(provider_id: String) -> PyResult<Self> {
        let path = resolve_default_config_path()?;
        let provider = build_provider(&path, &provider_id)?;
        Ok(Self::from_provider(provider_id, provider))
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
        thinking = None,
        response_schema = None,
        response_schema_strict = true,
        tags = None,
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
        thinking: Option<bool>,
        response_schema: Option<Bound<'_, PyDict>>,
        response_schema_strict: bool,
        tags: Option<Vec<String>>,
    ) -> PyResult<Response> {
        let req = build_request(
            model,
            user,
            system,
            messages,
            max_output_tokens,
            temperature,
            thinking,
            response_schema,
            response_schema_strict,
        )?;
        run_complete_tagged(py, self.inner.clone(), req, tags.unwrap_or_default())
    }
}

// ── Layer 2: Pipeline (middleware-wrapped) ────────────────────────────

/// Pipeline-wrapped service. Same `complete()` surface as [`Provider`]
/// but cache + retry + telemetry are engaged automatically. This is
/// the common case for production use — Python doesn't have to
/// re-implement any of those.
#[pyclass(frozen)]
pub(crate) struct Pipeline {
    id: String,
    inner: Arc<dyn LlmService>,
    capabilities_summary: CapabilitiesSummary,
    capabilities_full: tars_types::Capabilities,
    layer_names: Vec<String>,
    /// Set only for a `routed(..., policy="latency")` pipeline — the
    /// shared registry the bottom `RoutingService` feeds. Exposed via
    /// `Pipeline.latency_stats()`. `None` for single-provider pipelines.
    latency_stats: Option<Arc<LatencyStatsRegistry>>,
    /// Candidate provider ids a routed pipeline chooses among (in
    /// priority order). Single-element for non-routed pipelines.
    candidate_ids: Vec<String>,
}

impl Pipeline {
    pub(crate) fn inner_arc(&self) -> Arc<dyn LlmService> {
        self.inner.clone()
    }
    pub(crate) fn capabilities_owned(&self) -> tars_types::Capabilities {
        self.capabilities_full.clone()
    }

    /// Internal: wrap a built provider in TARS's default middleware
    /// stack (Telemetry → CacheLookup → Retry → Validation? →
    /// Provider). When `validators` is non-empty, ValidationMiddleware
    /// is layered between Retry and Provider so Reject{retriable=true}
    /// outcomes are caught by the existing RetryMiddleware. Shared
    /// between `from_config`, `from_str`, and `from_default`.
    fn from_provider(
        id: String,
        provider: Arc<dyn LlmProvider>,
        validators: Vec<Arc<dyn tars_pipeline::OutputValidator>>,
        event_stores: Option<EventStorePair>,
    ) -> Self {
        // The three `from_*` constructors keep the canonical defaults
        // (default retry, cache on). The builder path (`Pipeline.builder`)
        // routes through `from_provider_tuned` to override them.
        Self::from_provider_tuned(id, provider, validators, event_stores, None, true)
    }

    /// Like [`from_provider`] but with the builder-tunable knobs:
    /// `retry` override and `cache` toggle. Shared by the fluent
    /// `PipelineBuilder`.
    fn from_provider_tuned(
        id: String,
        provider: Arc<dyn LlmProvider>,
        validators: Vec<Arc<dyn tars_pipeline::OutputValidator>>,
        event_stores: Option<EventStorePair>,
        retry: Option<tars_pipeline::RetryConfig>,
        cache: bool,
    ) -> Self {
        let capabilities_full = provider.capabilities().clone();
        let capabilities_summary = CapabilitiesSummary::from(&capabilities_full);

        // Delegate the onion composition to `Pipeline::default_chain`
        // — same shape as before (EventEmitter? → Telemetry →
        // Validation? → Cache? → Retry → Provider), now expressed once
        // in tars-pipeline so non-Python callers (arc, tars-cli, etc.)
        // pick up the same canonical stack.
        let mut opts = tars_pipeline::PipelineOpts::new(ProviderId::new(id.clone()));
        opts.validators = validators;
        opts.events = event_stores
            .map(|EventStorePair { events, records }| tars_pipeline::EventStores { events, records });
        opts.retry = retry;
        opts.cache = cache;
        let pipeline = RsPipeline::default_chain(provider, opts);

        let layer_names: Vec<String> = pipeline
            .layer_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let inner: Arc<dyn LlmService> = Arc::new(pipeline);
        Self {
            id: id.clone(),
            inner,
            capabilities_summary,
            capabilities_full,
            layer_names,
            latency_stats: None,
            candidate_ids: vec![id],
        }
    }
}

/// Build the full multi-provider [`ProviderRegistry`] from a loaded
/// config (every configured provider, not just one). Used by the routed
/// pipeline constructor so a [`RoutingService`] can choose among them.
fn build_registry_from_cfg(cfg: &Config) -> PyResult<Arc<ProviderRegistry>> {
    let registry = ProviderRegistry::from_config_default(&cfg.providers)
        .map_err(|e| runtime_to_py("building provider registry", e))?;
    Ok(Arc::new(registry))
}

/// Bundle of stores wired into `EventEmitterMiddleware`. Constructed
/// from a directory path on the Python side; opens both files
/// underneath.
#[derive(Clone)]
pub(crate) struct EventStorePair {
    pub events: Arc<dyn tars_melt::event::PipelineEventLog>,
    pub records: Arc<dyn tars_melt::event::LlmRecordStore>,
}

impl EventStorePair {
    /// Open both stores under `dir`. Creates the directory if missing.
    /// Files: `{dir}/pipeline_events.db`, `{dir}/llm_records.db`.
    fn open_in_dir(dir: &str) -> PyResult<Self> {
        let dir_path = std::path::PathBuf::from(dir);
        // Validate the caller-supplied path before touching the filesystem:
        // require an absolute path and reject `..` traversal components so a
        // hostile/typo'd config can't create stores outside the intended root.
        if !dir_path.is_absolute() {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "event_store_dir must be an absolute path; got {dir:?}"
            )));
        }
        if dir_path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "event_store_dir must not contain `..` components; got {dir:?}"
            )));
        }
        if !dir_path.exists() {
            std::fs::create_dir_all(&dir_path)
                .map_err(|e| runtime_to_py("create event store dir", e))?;
        }
        let events_path = dir_path.join("pipeline_events.db");
        let records_path = dir_path.join("llm_records.db");

        let events: Arc<dyn tars_melt::event::PipelineEventLog> =
            tars_melt::event::SqlitePipelineEventLog::open(
                tars_melt::event::SqlitePipelineEventLogConfig::new(events_path),
            )
            .map_err(|e| runtime_to_py("open pipeline event store", e))?;
        let records: Arc<dyn tars_melt::event::LlmRecordStore> = tars_melt::event::SqliteLlmRecordStore::open(
            tars_melt::event::SqliteLlmRecordStoreConfig::new(records_path),
        )
        .map_err(|e| runtime_to_py("open llm record store", e))?;
        Ok(Self { events, records })
    }
}

/// Where a [`PipelineBuilder`] resolves its provider from.
enum ProviderSource {
    /// `~/.tars/config.toml`.
    Default,
    /// An explicit config file path.
    ConfigPath(String),
    /// Inline TOML text.
    ConfigStr(String),
}

/// Fluent builder returned by [`Pipeline::builder`]. Accumulates
/// layer config, then `build()` resolves the provider and assembles the
/// canonical onion via `Pipeline::default_chain`. The order is fixed
/// (load-bearing); the builder only toggles/configures layers.
#[pyclass]
pub(crate) struct PipelineBuilder {
    provider_id: String,
    source: ProviderSource,
    validators: Vec<Arc<dyn tars_pipeline::OutputValidator>>,
    event_store_dir: Option<String>,
    retry: Option<tars_pipeline::RetryConfig>,
    cache: bool,
}

#[pymethods]
impl PipelineBuilder {
    /// Attach output validators — same `[(name, callable), ...]` shape
    /// as the `from_*` `validators=` kwarg. Replaces any previously set.
    #[pyo3(signature = (validators = None))]
    fn validators<'py>(
        mut slf: PyRefMut<'py, Self>,
        validators: Option<Bound<'py, PyList>>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        slf.validators = validation::build_validator_list(validators)?;
        Ok(slf)
    }

    /// Enable the pipeline event store, writing to `dir` (same as the
    /// `event_store_dir=` kwarg).
    fn event_store(mut slf: PyRefMut<'_, Self>, dir: String) -> PyRefMut<'_, Self> {
        slf.event_store_dir = Some(dir);
        slf
    }

    /// Override the retry policy. Any argument left `None` keeps the
    /// current value (default: 3 attempts, 200ms→30s exp backoff ×2,
    /// 30s max wait). `max_attempts=1` disables retry.
    #[pyo3(signature = (
        *,
        max_attempts = None,
        initial_backoff_ms = None,
        max_backoff_ms = None,
        multiplier = None,
        max_wait_ms = None,
        respect_retry_after = None,
        max_attempts_maybe_retriable = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn retry<'py>(
        mut slf: PyRefMut<'py, Self>,
        max_attempts: Option<u32>,
        initial_backoff_ms: Option<u64>,
        max_backoff_ms: Option<u64>,
        multiplier: Option<f64>,
        max_wait_ms: Option<u64>,
        respect_retry_after: Option<bool>,
        max_attempts_maybe_retriable: Option<u32>,
    ) -> PyRefMut<'py, Self> {
        let mut cfg = slf.retry.clone().unwrap_or_default();
        if let Some(n) = max_attempts {
            cfg.max_attempts = n;
        }
        if let Some(ms) = initial_backoff_ms {
            cfg.initial_backoff = std::time::Duration::from_millis(ms);
        }
        if let Some(ms) = max_backoff_ms {
            cfg.max_backoff = std::time::Duration::from_millis(ms);
        }
        if let Some(m) = multiplier {
            cfg.multiplier = m;
        }
        if let Some(ms) = max_wait_ms {
            cfg.max_wait = std::time::Duration::from_millis(ms);
        }
        if let Some(b) = respect_retry_after {
            cfg.respect_retry_after = b;
        }
        if let Some(n) = max_attempts_maybe_retriable {
            cfg.max_attempts_maybe_retriable = n;
        }
        slf.retry = Some(cfg);
        slf
    }

    /// Include (`True`, default) or drop (`False`) the cache layer.
    /// Dropping it makes every call hit the provider.
    #[pyo3(signature = (enabled = true))]
    fn cache(mut slf: PyRefMut<'_, Self>, enabled: bool) -> PyRefMut<'_, Self> {
        slf.cache = enabled;
        slf
    }

    /// Resolve the provider and build the Pipeline. Can be called more
    /// than once (each call yields an independent Pipeline).
    fn build(&self) -> PyResult<Pipeline> {
        let provider = match &self.source {
            ProviderSource::Default => {
                let path = resolve_default_config_path()?;
                build_provider(&path, &self.provider_id)?
            }
            ProviderSource::ConfigPath(p) => build_provider(p, &self.provider_id)?,
            ProviderSource::ConfigStr(s) => build_provider_from_str(s, &self.provider_id)?,
        };
        let stores = self
            .event_store_dir
            .as_deref()
            .map(EventStorePair::open_in_dir)
            .transpose()?;
        Ok(Pipeline::from_provider_tuned(
            self.provider_id.clone(),
            provider,
            self.validators.clone(),
            stores,
            self.retry.clone(),
            self.cache,
        ))
    }

    fn __repr__(&self) -> String {
        format!(
            "PipelineBuilder(id={:?}, validators={}, event_store={}, retry_overridden={}, cache={})",
            self.provider_id,
            self.validators.len(),
            self.event_store_dir.is_some(),
            self.retry.is_some(),
            self.cache,
        )
    }
}

#[pymethods]
impl Pipeline {
    /// Construct a Pipeline from a TARS TOML config file.
    /// Wraps the named provider with TARS's default middleware stack
    /// (Telemetry → CacheLookup → Retry → Validation? → Provider).
    ///
    /// `validators` is a list of `(name: str, callable)` tuples
    /// where each callable has signature
    /// `(req: dict, resp: dict) -> Pass | Reject | FilterText | Annotate`.
    /// See `tars.validation` module docs.
    #[staticmethod]
    #[pyo3(signature = (path, provider_id, *, validators = None, event_store_dir = None))]
    fn from_config(
        path: String,
        provider_id: String,
        validators: Option<Bound<'_, PyList>>,
        event_store_dir: Option<String>,
    ) -> PyResult<Self> {
        let provider = build_provider(&path, &provider_id)?;
        let validators = validation::build_validator_list(validators)?;
        let stores = event_store_dir
            .as_deref()
            .map(EventStorePair::open_in_dir)
            .transpose()?;
        Ok(Self::from_provider(
            provider_id,
            provider,
            validators,
            stores,
        ))
    }

    /// Construct a Pipeline from inline TOML text. Equivalent to
    /// `from_config` but skips the round-trip through a tmpfile.
    /// See `from_config` for the `validators` and `event_store_dir`
    /// kwargs.
    #[staticmethod]
    #[pyo3(signature = (toml_text, provider_id, *, validators = None, event_store_dir = None))]
    fn from_str(
        toml_text: &str,
        provider_id: String,
        validators: Option<Bound<'_, PyList>>,
        event_store_dir: Option<String>,
    ) -> PyResult<Self> {
        let provider = build_provider_from_str(toml_text, &provider_id)?;
        let validators = validation::build_validator_list(validators)?;
        let stores = event_store_dir
            .as_deref()
            .map(EventStorePair::open_in_dir)
            .transpose()?;
        Ok(Self::from_provider(
            provider_id,
            provider,
            validators,
            stores,
        ))
    }

    /// Construct a Pipeline from the default user-level config at
    /// `~/.tars/config.toml`. Raises `TarsConfigError` if the file is
    /// missing — run `tars init` to bootstrap one. See `from_config`
    /// for the `validators` + `event_store_dir` kwargs.
    #[staticmethod]
    #[pyo3(signature = (provider_id, *, validators = None, event_store_dir = None))]
    fn from_default(
        provider_id: String,
        validators: Option<Bound<'_, PyList>>,
        event_store_dir: Option<String>,
    ) -> PyResult<Self> {
        let path = resolve_default_config_path()?;
        let provider = build_provider(&path, &provider_id)?;
        let validators = validation::build_validator_list(validators)?;
        let stores = event_store_dir
            .as_deref()
            .map(EventStorePair::open_in_dir)
            .transpose()?;
        Ok(Self::from_provider(
            provider_id,
            provider,
            validators,
            stores,
        ))
    }

    /// Start a fluent [`PipelineBuilder`] for finer control over the
    /// middleware stack than the `from_*` shortcuts give.
    ///
    /// The canonical onion **order is fixed** (it's load-bearing — e.g.
    /// Validation must sit outside Cache; see Doc 02 / B-20 W4), so the
    /// builder lets you *configure* and *opt out of* layers, not reorder
    /// them. What it adds over `from_default`:
    ///
    /// - `.retry(max_attempts=…, initial_backoff_ms=…, …)` — tune the
    ///   retry policy that `from_default` leaves at its default.
    /// - `.cache(False)` — drop the cache layer (every call hits the
    ///   provider).
    /// - `.validators([...])` / `.event_store(dir)` — same as the
    ///   `from_*` kwargs.
    ///
    /// Provider source defaults to `~/.tars/config.toml`; pass
    /// `config_path=` or `config_str=` to override (mutually exclusive).
    ///
    /// ```python
    /// p = (tars.Pipeline.builder("qwen_coder_local")
    ///         .retry(max_attempts=5, initial_backoff_ms=100)
    ///         .cache(False)
    ///         .build())
    /// ```
    #[staticmethod]
    #[pyo3(signature = (provider_id, *, config_path = None, config_str = None))]
    fn builder(
        provider_id: String,
        config_path: Option<String>,
        config_str: Option<String>,
    ) -> PyResult<PipelineBuilder> {
        if config_path.is_some() && config_str.is_some() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "pass at most one of config_path / config_str, not both",
            ));
        }
        let source = match (config_path, config_str) {
            (Some(p), _) => ProviderSource::ConfigPath(p),
            (_, Some(s)) => ProviderSource::ConfigStr(s),
            (None, None) => ProviderSource::Default,
        };
        Ok(PipelineBuilder {
            provider_id,
            source,
            validators: Vec::new(),
            event_store_dir: None,
            retry: None,
            cache: true,
        })
    }

    /// Construct a Pipeline that **routes** across several providers
    /// (B-8). The bottom of the onion becomes a `RoutingService` over
    /// `provider_ids` (in priority order) instead of a single provider;
    /// the usual Telemetry / Validation? / Cache? / Retry stack sits on
    /// top unchanged.
    ///
    /// `policy`:
    /// - `"latency"` (default) — try the historically-fastest provider
    ///   first, learned from observed dispatch latency. Read the running
    ///   numbers with [`Pipeline.latency_stats`]. `latency_metric`
    ///   chooses `"p50"` / `"p95"` (default) / `"mean"`.
    /// - `"cost"` — try the cheapest provider first, by *estimated* cost
    ///   of this request from each provider's static pricing (a free
    ///   local model wins; same token heuristic applies to all, so the
    ///   ordering is sound even though the absolute number is rough).
    /// - `"ensemble"` — hedged fan-out: dispatch to **all** providers in
    ///   parallel and take the first to respond (the rest are
    ///   cancelled). Trades extra calls for lower tail latency /
    ///   availability. "First response wins" — output-merging ensembles
    ///   (vote / best-of) are out of scope.
    /// - `"static"` — try `provider_ids` in the given order (fallback
    ///   chain only; no reordering).
    ///
    /// `cache` defaults to **False** here: a cache shared across
    /// providers could serve one provider's response when another was
    /// routed to. Enable it only when the candidates are
    /// interchangeable (same model). Provider source / `validators` /
    /// `event_store_dir` work as in `from_default`.
    #[staticmethod]
    #[pyo3(signature = (
        provider_ids,
        *,
        policy = "latency",
        latency_metric = "p95",
        config_path = None,
        config_str = None,
        cache = false,
        validators = None,
        event_store_dir = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn routed(
        provider_ids: Vec<String>,
        policy: &str,
        latency_metric: &str,
        config_path: Option<String>,
        config_str: Option<String>,
        cache: bool,
        validators: Option<Bound<'_, PyList>>,
        event_store_dir: Option<String>,
    ) -> PyResult<Self> {
        if provider_ids.is_empty() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "routed() needs at least one provider id",
            ));
        }
        if config_path.is_some() && config_str.is_some() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "pass at most one of config_path / config_str, not both",
            ));
        }
        let cfg = match (config_path, config_str) {
            (Some(p), _) => ConfigManager::load_from_file(&p).map_err(config_to_py)?,
            (_, Some(s)) => ConfigManager::load_from_str(&s).map_err(config_to_py)?,
            (None, None) => ConfigManager::load_from_file(&resolve_default_config_path()?)
                .map_err(config_to_py)?,
        };
        let registry = build_registry_from_cfg(&cfg)?;

        // Validate every candidate up front so a typo'd id is a clean
        // error, not a silent skip at dispatch time.
        let mut pids = Vec::with_capacity(provider_ids.len());
        for id in &provider_ids {
            let pid = ProviderId::new(id.clone());
            if registry.get(&pid).is_none() {
                let configured: Vec<String> =
                    cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
                return Err(crate::errors::TarsConfigError::new_err(format!(
                    "provider {id:?} not in config. Configured: [{}]",
                    configured.join(", "),
                )));
            }
            pids.push(pid);
        }

        // Capabilities of the first candidate represent the routed
        // pipeline (routing picks dynamically; the primary is the
        // sensible default for `check_compatibility`).
        let first = registry.get(&pids[0]).expect("validated above");
        let capabilities_full = first.capabilities().clone();
        let capabilities_summary = CapabilitiesSummary::from(&capabilities_full);

        let base = Arc::new(StaticPolicy::new(pids.clone()).map_err(provider_to_py)?);
        let (routing, latency_stats): (Arc<dyn LlmService>, Option<Arc<LatencyStatsRegistry>>) =
            match policy {
                "static" => (RoutingService::new(registry, base), None),
                "ensemble" => {
                    // Hedged fan-out: dispatch all candidates in
                    // parallel, first response wins. Not a RoutingPolicy
                    // (it's a different dispatch shape), so `base` is
                    // unused on this arm.
                    (EnsembleService::new(registry, pids.clone()), None)
                }
                "cost" => {
                    // Snapshot each candidate's static pricing from the
                    // registry; CostPolicy sorts cheapest-first from it.
                    let mut prices = std::collections::HashMap::new();
                    for pid in &pids {
                        if let Some(p) = registry.get(pid) {
                            prices.insert(pid.clone(), p.capabilities().pricing);
                        }
                    }
                    let pol = Arc::new(CostPolicy::new(base, prices));
                    (RoutingService::new(registry, pol), None)
                }
                "latency" => {
                    let metric = match latency_metric {
                        "p50" => LatencyMetric::P50,
                        "p95" => LatencyMetric::P95,
                        "mean" => LatencyMetric::Mean,
                        other => {
                            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                                "unknown latency_metric {other:?}; use p50 / p95 / mean"
                            )));
                        }
                    };
                    let stats = Arc::new(LatencyStatsRegistry::new(100));
                    let pol = Arc::new(LatencyPolicy::new(base, stats.clone()).with_metric(metric));
                    (
                        RoutingService::with_latency_stats(registry, pol, stats.clone()),
                        Some(stats),
                    )
                }
                other => {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "unknown policy {other:?}; use \"latency\", \"cost\", \"ensemble\", or \"static\""
                    )));
                }
            };

        let validators = validation::build_validator_list(validators)?;
        let stores = event_store_dir
            .as_deref()
            .map(EventStorePair::open_in_dir)
            .transpose()?;

        // `cache_origin` is the cache namespace — a synthetic id for the
        // routed set (cache defaults off here anyway; see the doc above).
        let mut opts = tars_pipeline::PipelineOpts::new(ProviderId::new("routed"));
        opts.validators = validators;
        opts.events = stores
            .map(|EventStorePair { events, records }| tars_pipeline::EventStores { events, records });
        opts.cache = cache;
        let pipeline = RsPipeline::chain_over(routing, opts);
        let layer_names: Vec<String> = pipeline
            .layer_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        Ok(Self {
            id: format!("routed[{}]", provider_ids.join(",")),
            inner: Arc::new(pipeline),
            capabilities_summary,
            capabilities_full,
            layer_names,
            latency_stats,
            candidate_ids: provider_ids,
        })
    }

    /// Per-provider observed latency for a `routed(policy="latency")`
    /// pipeline: `{provider_id: {count, mean_ms, p50_ms, p95_ms}}`.
    /// Empty for non-routed / static pipelines, or before any call has
    /// been dispatched to a given provider.
    fn latency_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        if let Some(stats) = &self.latency_stats {
            for id in &self.candidate_ids {
                if let Some(s) = stats.snapshot(&ProviderId::new(id.clone())) {
                    let entry = PyDict::new(py);
                    entry.set_item("count", s.count)?;
                    entry.set_item("mean_ms", s.mean_ms)?;
                    entry.set_item("p50_ms", s.p50_ms)?;
                    entry.set_item("p95_ms", s.p95_ms)?;
                    d.set_item(id, entry)?;
                }
            }
        }
        Ok(d)
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
        format!("Pipeline(id={:?}, layers={:?})", self.id, self.layer_names,)
    }

    #[pyo3(signature = (
        model,
        *,
        user = None,
        system = None,
        messages = None,
        max_output_tokens = None,
        temperature = None,
        thinking = None,
        response_schema = None,
        response_schema_strict = true,
        tags = None,
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
        thinking: Option<bool>,
        response_schema: Option<Bound<'_, PyDict>>,
        response_schema_strict: bool,
        tags: Option<Vec<String>>,
    ) -> PyResult<Response> {
        let req = build_request(
            model,
            user,
            system,
            messages,
            max_output_tokens,
            temperature,
            thinking,
            response_schema,
            response_schema_strict,
        )?;
        run_complete_tagged(py, self.inner.clone(), req, tags.unwrap_or_default())
    }

    /// Pre-flight check: would this Pipeline's underlying provider
    /// accept this request, given its capabilities? Lets caller
    /// short-circuit BEFORE incurring a network round-trip when the
    /// answer is "obviously no" (e.g. provider doesn't support tools
    /// but the request includes some).
    ///
    /// Returns a `CompatibilityResult` with `.is_compatible: bool` and
    /// `.reasons: list[CompatibilityReason]`. Each reason has `.kind`
    /// (snake_case tag for branching), `.message` (human-readable
    /// Display), and structured fields per kind.
    ///
    /// Same kwargs as `complete()` minus the streaming concerns —
    /// returns a verdict synchronously, no model call.
    ///
    /// **For config-time checks** (no real prompt yet, just need to
    /// verify "does this provider support tools at all?"), use the
    /// lighter [`Pipeline::check_capabilities_for`] which doesn't
    /// require building a full ChatRequest.
    #[pyo3(signature = (
        model,
        *,
        user = None,
        system = None,
        messages = None,
        max_output_tokens = None,
        temperature = None,
        thinking = None,
        response_schema = None,
        response_schema_strict = true,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn check_compatibility(
        &self,
        model: String,
        user: Option<String>,
        system: Option<String>,
        messages: Option<Bound<'_, PyList>>,
        max_output_tokens: Option<u32>,
        temperature: Option<f32>,
        thinking: Option<bool>,
        response_schema: Option<Bound<'_, PyDict>>,
        response_schema_strict: bool,
    ) -> PyResult<CompatibilityResult> {
        let req = build_request(
            model,
            user,
            system,
            messages,
            max_output_tokens,
            temperature,
            thinking,
            response_schema,
            response_schema_strict,
        )?;
        Ok(compatibility_to_py(
            req.compatibility_check(&self.capabilities_full),
        ))
    }

    /// **Config-time** capability check (kwargs ergonomic form).
    /// Declarative — caller states what features they need (no real
    /// prompt required). Use this at startup to verify each role's
    /// configured provider satisfies the role's needs, without the
    /// "configure → fail at runtime → fall back" loop.
    ///
    /// All kwargs default to "I don't need this" (False / 0); set
    /// only the axes you actually depend on. **For typed callers
    /// who want IDE / mypy field-name validation**, build a
    /// `CapabilityRequirements` and pass via `check_capabilities`
    /// instead — kwargs are convenient inline but stringly-typed
    /// (a typo like `requires_struuctured_output=True` is silently
    /// accepted by `**unpack` semantics, surfacing as a runtime
    /// "unexpected keyword argument" far from the typo site).
    ///
    /// ```python
    /// # Inline / quick check:
    /// r = p.check_capabilities_for(requires_tools=True)
    ///
    /// # Typed (recommended):
    /// reqs = tars.CapabilityRequirements(
    ///     requires_tools=True,
    ///     requires_thinking=(role == "planner"),
    ///     estimated_max_prompt_tokens=8000,
    /// )
    /// r = p.check_capabilities(reqs)
    /// ```
    ///
    /// Returns the same `CompatibilityResult` type as
    /// `check_compatibility` — same downstream branching code works
    /// for all 3 APIs.
    #[pyo3(signature = (
        *,
        requires_tools = false,
        requires_vision = false,
        requires_thinking = false,
        requires_structured_output = false,
        estimated_max_prompt_tokens = 0,
        estimated_max_output_tokens = 0,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn check_capabilities_for(
        &self,
        requires_tools: bool,
        requires_vision: bool,
        requires_thinking: bool,
        requires_structured_output: bool,
        estimated_max_prompt_tokens: u32,
        estimated_max_output_tokens: u32,
    ) -> CompatibilityResult {
        let reqs = tars_types::CapabilityRequirements {
            requires_tools,
            requires_vision,
            requires_thinking,
            requires_structured_output,
            estimated_max_prompt_tokens,
            estimated_max_output_tokens,
        };
        compatibility_to_py(self.capabilities_full.check_requirements(&reqs))
    }

    /// **Typed-input variant** of [`check_capabilities_for`]. Takes a
    /// `CapabilityRequirements` instance instead of kwargs — gives
    /// callers IDE autocomplete + mypy field-name validation +
    /// dataclass-style construction.
    ///
    /// `tars.CapabilityRequirements` is the single source of truth
    /// for the requirement axes; Consumers should
    /// build their `role → CapabilityRequirements` mapping using
    /// this class rather than mirroring fields locally (which
    /// drifts on tars upgrades).
    fn check_capabilities(&self, requirements: &CapabilityRequirementsPy) -> CompatibilityResult {
        let reqs = tars_types::CapabilityRequirements {
            requires_tools: requirements.requires_tools,
            requires_vision: requirements.requires_vision,
            requires_thinking: requirements.requires_thinking,
            requires_structured_output: requirements.requires_structured_output,
            estimated_max_prompt_tokens: requirements.estimated_max_prompt_tokens,
            estimated_max_output_tokens: requirements.estimated_max_output_tokens,
        };
        compatibility_to_py(self.capabilities_full.check_requirements(&reqs))
    }
}

/// Typed declarative requirements for `Pipeline.check_capabilities`.
/// Ships as a frozen pyclass with named, typed fields so Python
/// callers get IDE autocomplete + mypy validation + reliable
/// `dataclass-style` construction.
///
/// **Why first-class type vs. plain dict / kwargs**:
///
/// - Field name typos caught at construction (kwargs `**unpack`
///   raises far from the typo site).
/// - Field types enforced (kwargs `requires_tools="yes"` would pass
///   loosely-typed dict but fail here).
/// - `is_empty()` lets caller short-circuit pre-flight when no axes
///   were declared (factory pattern: don't run a no-op check).
/// - Single source of truth — when tars adds a capability axis,
///   all consumers of this type automatically pick up the new field
///   on rebuild; no per-consumer mirror to drift.
///
/// Consumers building their `role → requirements` mapping
/// should import this type rather than declare a local mirror:
///
/// ```python
/// from tars import CapabilityRequirements
/// ROLE_REQUIREMENTS: dict[str, CapabilityRequirements] = {
///     "critic":  CapabilityRequirements(requires_tools=True),
///     "planner": CapabilityRequirements(requires_thinking=True),
/// }
/// ```
#[pyclass(frozen, get_all, name = "CapabilityRequirements")]
#[derive(Clone, Debug, Default)]
pub(crate) struct CapabilityRequirementsPy {
    pub(crate) requires_tools: bool,
    pub(crate) requires_vision: bool,
    pub(crate) requires_thinking: bool,
    pub(crate) requires_structured_output: bool,
    pub(crate) estimated_max_prompt_tokens: u32,
    pub(crate) estimated_max_output_tokens: u32,
}

#[pymethods]
impl CapabilityRequirementsPy {
    #[new]
    #[pyo3(signature = (
        *,
        requires_tools = false,
        requires_vision = false,
        requires_thinking = false,
        requires_structured_output = false,
        estimated_max_prompt_tokens = 0,
        estimated_max_output_tokens = 0,
    ))]
    fn new(
        requires_tools: bool,
        requires_vision: bool,
        requires_thinking: bool,
        requires_structured_output: bool,
        estimated_max_prompt_tokens: u32,
        estimated_max_output_tokens: u32,
    ) -> Self {
        Self {
            requires_tools,
            requires_vision,
            requires_thinking,
            requires_structured_output,
            estimated_max_prompt_tokens,
            estimated_max_output_tokens,
        }
    }

    /// True iff all axes are defaults (no actual requirement set).
    /// Useful for the factory pattern — skip pre-flight when the
    /// requirements set is empty:
    ///
    /// ```python
    /// reqs = role_requirements(role)   # may return empty
    /// if not reqs.is_empty():
    ///     pipeline.check_capabilities(reqs)
    /// ```
    fn is_empty(&self) -> bool {
        !self.requires_tools
            && !self.requires_vision
            && !self.requires_thinking
            && !self.requires_structured_output
            && self.estimated_max_prompt_tokens == 0
            && self.estimated_max_output_tokens == 0
    }

    /// Convert to a kwargs dict — for callers that have a typed
    /// `CapabilityRequirements` but want to pass it through the
    /// kwargs-style `check_capabilities_for(**kwargs)` API. Lets
    /// existing call sites adopt the typed class incrementally
    /// without changing the call shape.
    fn to_kwargs<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("requires_tools", self.requires_tools)?;
        d.set_item("requires_vision", self.requires_vision)?;
        d.set_item("requires_thinking", self.requires_thinking)?;
        d.set_item(
            "requires_structured_output",
            self.requires_structured_output,
        )?;
        d.set_item(
            "estimated_max_prompt_tokens",
            self.estimated_max_prompt_tokens,
        )?;
        d.set_item(
            "estimated_max_output_tokens",
            self.estimated_max_output_tokens,
        )?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "CapabilityRequirements(requires_tools={}, requires_vision={}, \
             requires_thinking={}, requires_structured_output={}, \
             estimated_max_prompt_tokens={}, estimated_max_output_tokens={})",
            self.requires_tools,
            self.requires_vision,
            self.requires_thinking,
            self.requires_structured_output,
            self.estimated_max_prompt_tokens,
            self.estimated_max_output_tokens,
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.requires_tools == other.requires_tools
            && self.requires_vision == other.requires_vision
            && self.requires_thinking == other.requires_thinking
            && self.requires_structured_output == other.requires_structured_output
            && self.estimated_max_prompt_tokens == other.estimated_max_prompt_tokens
            && self.estimated_max_output_tokens == other.estimated_max_output_tokens
    }

    fn __hash__(&self) -> u64 {
        // Cheap deterministic hash so dataclass-like patterns work
        // (using as dict key, set member, etc.).
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.requires_tools.hash(&mut h);
        self.requires_vision.hash(&mut h);
        self.requires_thinking.hash(&mut h);
        self.requires_structured_output.hash(&mut h);
        self.estimated_max_prompt_tokens.hash(&mut h);
        self.estimated_max_output_tokens.hash(&mut h);
        h.finish()
    }
}

/// Verdict from `Pipeline.check_compatibility()`. Mirrors
/// `tars_types::CompatibilityCheck`.
#[pyclass(frozen, name = "CompatibilityResult")]
#[derive(Debug)]
pub(crate) struct CompatibilityResult {
    pub(crate) is_compatible: bool,
    pub(crate) reasons: Vec<CompatibilityReasonPy>,
}

#[pymethods]
impl CompatibilityResult {
    #[getter]
    fn is_compatible(&self) -> bool {
        self.is_compatible
    }
    #[getter]
    fn reasons(&self, py: Python<'_>) -> PyResult<Py<PyList>> {
        let list = PyList::empty(py);
        for r in &self.reasons {
            let item = Py::new(
                py,
                CompatibilityReasonPy {
                    kind: r.kind.clone(),
                    message: r.message.clone(),
                    detail_json: r.detail_json.clone(),
                },
            )?;
            list.append(item)?;
        }
        Ok(list.unbind())
    }
    fn __repr__(&self) -> String {
        if self.is_compatible {
            "CompatibilityResult(Compatible)".into()
        } else {
            let kinds: Vec<&str> = self.reasons.iter().map(|r| r.kind.as_str()).collect();
            format!("CompatibilityResult(Incompatible, kinds={kinds:?})")
        }
    }
    fn __bool__(&self) -> bool {
        self.is_compatible
    }
}

/// One incompatibility reason. `kind` is the stable snake_case tag
/// (`"tool_use"` / `"vision"` / `"context_window"` / etc.); `message`
/// is the human-readable Display; `detail` is a kind-specific dict
/// with structured fields where applicable (e.g. `{"estimated_tokens":
/// 50000, "max_tokens": 32768}` for `context_window`). Stored as JSON
/// internally so the type is `Clone`-able for PyO3 getter generation;
/// converted to a Python dict on demand via the `detail` getter.
/// `kind` is `String` deliberately: same rationale as
/// [`RetryAttemptPy::kind`] — the field crosses into Python as a
/// string and the value is sourced from the typed
/// `CompatibilityCheck` enum's stable kind name.
#[pyclass(frozen, name = "CompatibilityReason")]
#[derive(Debug)]
pub(crate) struct CompatibilityReasonPy {
    pub(crate) kind: String,
    pub(crate) message: String,
    pub(crate) detail_json: Option<serde_json::Value>,
}

#[pymethods]
impl CompatibilityReasonPy {
    #[getter]
    fn kind(&self) -> &str {
        &self.kind
    }
    #[getter]
    fn message(&self) -> &str {
        &self.message
    }
    #[getter]
    fn detail<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        match &self.detail_json {
            None => Ok(None),
            Some(v) => {
                let json_mod = py.import("json")?;
                // Propagate a serialization failure rather than silently
                // handing Python an empty `{}` — a `{}` would masquerade as
                // a valid (empty) detail and hide the real failure.
                let s = serde_json::to_string(v).map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(format!(
                        "failed to serialize compatibility reason detail: {e}"
                    ))
                })?;
                let obj = json_mod.call_method1("loads", (s,))?;
                Ok(Some(obj.downcast_into::<PyDict>()?))
            }
        }
    }
    fn __repr__(&self) -> String {
        format!(
            "CompatibilityReason(kind={:?}, message={:?})",
            self.kind, self.message
        )
    }
}

fn compatibility_to_py(c: tars_types::CompatibilityCheck) -> CompatibilityResult {
    use tars_types::{CompatibilityCheck, CompatibilityReason};
    match c {
        CompatibilityCheck::Compatible => CompatibilityResult {
            is_compatible: true,
            reasons: Vec::new(),
        },
        CompatibilityCheck::Incompatible { reasons } => {
            let py_reasons = reasons
                .into_iter()
                .map(|r| {
                    let kind = r.kind().to_string();
                    let message = r.to_string();
                    let detail_json = match &r {
                        CompatibilityReason::ToolUseUnsupported { tool_count } => {
                            Some(serde_json::json!({"tool_count": *tool_count}))
                        }
                        CompatibilityReason::ThinkingUnsupported { mode } => {
                            Some(serde_json::json!({"mode": format!("{mode:?}")}))
                        }
                        CompatibilityReason::ContextWindowExceeded {
                            estimated_prompt_tokens,
                            max_context_tokens,
                        } => Some(serde_json::json!({
                            "estimated_prompt_tokens": *estimated_prompt_tokens,
                            "max_context_tokens": *max_context_tokens,
                        })),
                        CompatibilityReason::MaxOutputTokensExceeded { requested, max } => {
                            Some(serde_json::json!({
                                "requested": *requested,
                                "max": *max,
                            }))
                        }
                        CompatibilityReason::StructuredOutputUnsupported
                        | CompatibilityReason::VisionUnsupported => None,
                        // `#[non_exhaustive]` wildcard.
                        _ => None,
                    };
                    CompatibilityReasonPy {
                        kind,
                        message,
                        detail_json,
                    }
                })
                .collect();
            CompatibilityResult {
                is_compatible: false,
                reasons: py_reasons,
            }
        }
        // `#[non_exhaustive]` wildcard. Fail *closed*: an unknown future
        // variant might be a new incompatibility this build can't model
        // (e.g. a critical-incompat kind). Reporting it as compatible
        // would let a request proceed that should be blocked, so we
        // surface it as incompatible with a self-describing reason and
        // let the caller decide (the fix is to upgrade tars).
        ref other => {
            tracing::warn!(
                check = ?other,
                "compatibility_to_py: unknown CompatibilityCheck variant treated as \
                 incompatible (tars-py is older than tars-types — upgrade to resolve)",
            );
            CompatibilityResult {
                is_compatible: false,
                reasons: vec![CompatibilityReasonPy {
                    kind: "unknown_compatibility_check".to_string(),
                    message: format!(
                        "provider reported a compatibility variant this build does not \
                         understand ({other:?}); treating as incompatible"
                    ),
                    detail_json: None,
                }],
            }
        }
    }
}

// ── Shared helpers ────────────────────────────────────────────────────

/// Resolve the user-level default config path
/// (`~/.tars/config.toml`) to a string. Errors out as
/// `TarsConfigError` if the home directory is unknowable — exceedingly
/// rare in normal use, but possible on locked-down hosts.
fn resolve_default_config_path() -> PyResult<String> {
    let path = default_config_path().ok_or_else(|| {
        crate::errors::TarsConfigError::new_err(
            "could not resolve home directory; set $HOME or pass an explicit config path",
        )
    })?;
    Ok(path.to_string_lossy().into_owned())
}

/// Python-exposed: returns the default config path as a string.
/// Equivalent to `tars_config::default_config_path()` resolved to a
/// platform-native path. Returns `None` if home dir is unknowable.
#[pyfunction(name = "default_config_path")]
fn default_config_path_py() -> Option<String> {
    default_config_path().map(|p| p.to_string_lossy().into_owned())
}

/// Build the named provider from an already-loaded `Config`. Shared
/// between the `from_config` (file-path) and `from_str` (inline TOML)
/// constructors so error mapping stays uniform.
fn build_provider_from_cfg(cfg: &Config, provider_id: &str) -> PyResult<Arc<dyn LlmProvider>> {
    let registry = ProviderRegistry::from_config_default(&cfg.providers)
        .map_err(|e| runtime_to_py("building provider registry", e))?;
    let pid = ProviderId::new(provider_id.to_string());
    registry.get(&pid).ok_or_else(|| {
        let configured: Vec<String> = cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
        // Unknown provider id is a config-shaped error (the file is
        // valid TOML; the caller just named something not there), so
        // it's TarsConfigError rather than TarsProviderError.
        crate::errors::TarsConfigError::new_err(format!(
            "provider {provider_id:?} not in config. Configured: [{}]",
            configured.join(", "),
        ))
    })
}

/// Load config from disk, then resolve the named provider.
fn build_provider(path: &str, provider_id: &str) -> PyResult<Arc<dyn LlmProvider>> {
    let cfg = ConfigManager::load_from_file(path).map_err(config_to_py)?;
    build_provider_from_cfg(&cfg, provider_id)
}

/// Same as [`build_provider`] but parses inline TOML — for tests and
/// programmatic construction without round-tripping through a tmpfile.
fn build_provider_from_str(toml_text: &str, provider_id: &str) -> PyResult<Arc<dyn LlmProvider>> {
    let cfg = ConfigManager::load_from_str(toml_text).map_err(config_to_py)?;
    build_provider_from_cfg(&cfg, provider_id)
}

/// Build a [`ChatRequest`] from Python kwargs. Accepts either:
/// - `user="..."` for a single-turn user message, OR
/// - `messages=[{role, content}, ...]` for multi-turn,
/// - `system="..."` always optional (top-level system prompt),
/// - `thinking=True/False` to set the OpenAI-compat
///   `chat_template_kwargs.enable_thinking` per-request override
///   (Qwen3 / mlx_lm.server / vLLM). `None` = field omitted,
///   server's chat-template default decides.
/// - `response_schema=<dict>` — JSON Schema for the response. Triggers
///   constrained decoding at the provider when supported (OpenAI / LM
///   Studio: `response_format={type:json_schema,...}`; Anthropic:
///   forced tool_use emulation; Gemini: `response_schema`). Eliminates
///   common "model returns invalid JSON" failures at the source.
/// - `response_schema_strict=True` (default) — when `response_schema`
///   is set, request strict provider-side enforcement. `False` falls
///   back to loose mode where the schema is a hint rather than a hard
///   GBNF/grammar constraint. **Diagnostic toggle**: some
///   model+server combinations (notably LM Studio + Qwen3-Coder-30B
///   on Q4 quant) suffer recall regressions under strict mode; loose
///   mode preserves model freedom while still steering toward the
///   intended shape. Has no effect when `response_schema` is `None`.
#[allow(clippy::too_many_arguments)]
fn build_request(
    model: String,
    user: Option<String>,
    system: Option<String>,
    messages: Option<Bound<'_, PyList>>,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
    thinking: Option<bool>,
    response_schema: Option<Bound<'_, PyDict>>,
    response_schema_strict: bool,
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
        return Err(PyValueError::new_err("must pass `user=` or `messages=`"));
    };

    // Convert the Python dict (JSON Schema doc) into tars's JsonSchema.
    // `strict` toggle exposed because some model+server stacks (LM
    // Studio's GBNF on Qwen3-Coder-30B Q4 in particular) suffer recall
    // regressions under strict GBNF — loose mode lets the model emit
    // schema-ish-but-not-grammar-constrained JSON, which often
    // preserves quality while still steering shape.
    let structured_output = match response_schema {
        Some(dict) => {
            let py = dict.py();
            let json_mod = py.import("json")?;
            let schema_str: String = json_mod.call_method1("dumps", (dict,))?.extract()?;
            let schema_value: serde_json::Value = serde_json::from_str(&schema_str)
                .map_err(|e| PyValueError::new_err(format!("invalid response_schema dict: {e}")))?;
            Some(if response_schema_strict {
                tars_types::JsonSchema::strict("Response", schema_value)
            } else {
                tars_types::JsonSchema::loose(schema_value)
            })
        }
        None => None,
    };

    Ok(ChatRequest {
        model: ModelHint::Explicit(model),
        system,
        messages: msgs,
        tools: Vec::new(),
        tool_choice: Default::default(),
        structured_output,
        max_output_tokens,
        temperature,
        stop_sequences: Vec::new(),
        seed: None,
        cache_directives: Vec::new(),
        thinking: tars_types::ThinkingMode::default(),
        enable_chat_template_thinking: thinking,
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
        "assistant" => Ok(Message::Assistant {
            content: blocks,
            tool_calls: Vec::new(),
        }),
        "system" => Ok(Message::System { content: blocks }),
        "tool" => Err(PyValueError::new_err(
            "tool-result messages need a tool_call_id; not supported via this convenience surface yet",
        )),
        other => Err(PyValueError::new_err(format!("unknown role: {other}"))),
    }
}

/// Drain the LLM stream into a [`Response`]. Stamps `tags` onto
/// `RequestContext.tags` — propagated to `PipelineEvent.tags` for
/// SQL rollups (`WHERE 'dogfood_X' = ANY(tags)`). Pass empty `tags`
/// for the no-cohort path. Releases the GIL while waiting on the
/// async runtime so other Python threads keep working.
fn run_complete_tagged(
    py: Python<'_>,
    svc: Arc<dyn LlmService>,
    req: ChatRequest,
    tags: Vec<String>,
) -> PyResult<Response> {
    let rt = tokio_runtime()?;
    // Read the active `with handle.context(...)` off the thread-local BEFORE
    // releasing the GIL / entering the runtime — the task-local does not
    // survive the FFI hop, so the binding re-establishes the scope per call
    // (Doc 12 §6.2). `None` (no active context, or a direct
    // `Pipeline.complete` outside a handle) keeps the prior test-default
    // behaviour, so existing call sites are unchanged.
    let active_ctx = context::current();
    py.allow_threads(|| {
        rt.block_on(async move {
            // Build the context here (rather than inline in `call`) so
            // we keep an Arc clone of the telemetry handle that
            // survives the move into the middleware chain. Middleware
            // writes through the same Arc; we read it back after
            // stream drain.
            let ctx = match active_ctx {
                // Cohort tags passed to `complete(tags=…)` override the
                // context's tags for this one call; an empty list leaves the
                // context tags in place.
                Some(c) if tags.is_empty() => c,
                Some(c) => c.with_tags(tags),
                None => RequestContext::test_default().with_tags(tags),
            };
            let telemetry_handle = ctx.telemetry.clone();
            let validation_handle = ctx.validation_outcome.clone();

            // The pipeline reads its ctx from the call argument; we also
            // re-scope `RUN_CONTEXT` around the drain so any deep code /
            // detached job (`spawn_with_context`) sees the same context. Both
            // share the ctx's Arcs, so the telemetry/validation handles read
            // back the middleware's writes.
            let call_ctx = ctx.clone();
            let drain = async move {
            let mut stream = svc.call(req, call_ctx).await.map_err(provider_to_py)?;
            let mut builder = ChatResponseBuilder::new();
            while let Some(ev) = stream.next().await {
                let ev = ev.map_err(provider_to_py)?;
                builder.apply(ev);
            }
            let mut resp = builder.finish();

            // ValidationMiddleware (when present) publishes its
            // post-Filter response + summary on the side-channel; prefer
            // that over the stream-rebuild so `validation_summary` is
            // populated and any Filter outcome is reflected.
            // A poisoned lock here means a validator panicked while
            // holding it. Falling through would return the *unfiltered*
            // `builder.finish()` response — silently bypassing any
            // FilterText/Reject outcome (PII scrub, content moderation,
            // …). Refuse to serve potentially-unvalidated content.
            match validation_handle.lock() {
                Ok(rec) => {
                    if let Some(filtered) = &rec.filtered_response {
                        resp = filtered.clone();
                    }
                }
                Err(_) => {
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(
                        "validation state lock poisoned — refusing to return a \
                         possibly-unvalidated response (a validator likely panicked)",
                    ));
                }
            }

            let telemetry = read_telemetry(&telemetry_handle);
            let validation_summary = validation_summary_to_py(resp.validation_summary.clone());

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
                telemetry,
                validation_summary,
            })
            };
            RUN_CONTEXT.scope(ctx, drain).await
        })
    })
}

/// Snapshot the shared telemetry into the immutable Python-facing
/// `Telemetry` struct. Public for use by Session's per-call response
/// path too.
pub(crate) fn read_telemetry(handle: &tars_types::SharedTelemetry) -> Telemetry {
    let acc = match handle.lock() {
        Ok(g) => g.clone(),
        // Mutex poisoned (some prior holder panicked) — very rare.
        // Surface a default so the call still returns a usable
        // Response rather than tearing down on a metadata-only lock
        // error. The text + usage above are the contract; telemetry
        // is the bonus. Log it, though: a poisoned lock means a panic
        // in the middleware stack, and the returned telemetry is then
        // fabricated defaults rather than real data.
        Err(e) => {
            tracing::warn!(
                error = %e,
                "read_telemetry: telemetry mutex poisoned (a middleware layer likely \
                 panicked); returning default/empty telemetry",
            );
            tars_types::TelemetryAccumulator::default()
        }
    };
    Telemetry {
        cache_hit: acc.cache_hit,
        retry_count: acc.retry_count,
        retry_attempts: acc
            .retry_attempts
            .into_iter()
            .map(|r| RetryAttemptPy {
                kind: r.error_kind,
                retry_after_ms: r.retry_after_ms,
            })
            .collect(),
        provider_latency_ms: acc.provider_latency_ms,
        pipeline_total_ms: acc.pipeline_total_ms,
        layers: acc.layers,
    }
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

/// Public re-export so the validation submodule can format
/// stop_reason consistently with `Response.stop_reason`.
pub(crate) fn stop_reason_str_pub(r: &StopReason) -> &'static str {
    stop_reason_str(r)
}

// ── Module ────────────────────────────────────────────────────────────

#[pyfunction]
fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Load a committed bless file and check a completion's text against it
/// (Doc 28). `text` is decoded as JSON (chatty-tolerant), then each blessed
/// field is asserted. Returns
/// `{"passed": bool, "drifts": [{"selector","expected","actual","reason"}]}`.
/// `expected`/`actual` are JSON-encoded strings so any value shape survives.
#[pyfunction]
fn bless_check<'py>(py: Python<'py>, path: String, text: String) -> PyResult<Bound<'py, PyDict>> {
    let value: serde_json::Value =
        tars_utils::decode_json(&text, tars_types::StructuredOutputMode::None)
            .map_err(|e| crate::errors::TarsProviderError::new_err(format!("bless decode: {e}")))?;
    let bless = tars_types::Bless::load(std::path::Path::new(&path))
        .map_err(|e| crate::errors::TarsConfigError::new_err(format!("bless load: {e}")))?;
    let outcome = bless
        .check(&value)
        .map_err(|e| crate::errors::TarsConfigError::new_err(format!("bless check: {e}")))?;
    let d = PyDict::new(py);
    d.set_item("passed", outcome.is_pass())?;
    let drifts = PyList::empty(py);
    for dr in &outcome.drifts {
        let dd = PyDict::new(py);
        dd.set_item("selector", &dr.selector)?;
        dd.set_item("expected", dr.expected.to_string())?;
        dd.set_item("actual", dr.actual.as_ref().map(|v| v.to_string()))?;
        dd.set_item("reason", &dr.reason)?;
        drifts.append(dd)?;
    }
    d.set_item("drifts", drifts)?;
    Ok(d)
}

/// PyO3 module entry point. Symbol must be `_tars_py` to match
/// `pyproject.toml`'s `module-name = "tars._tars_py"`. Public Python
/// surface is curated by `python/tars/__init__.py`.
#[pymodule]
fn _tars_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(bless_check, m)?)?;
    m.add_function(wrap_pyfunction!(default_config_path_py, m)?)?;
    m.add_function(wrap_pyfunction!(eval::write_score, m)?)?;
    m.add_function(wrap_pyfunction!(eval::read_calls, m)?)?;
    // Config + runtime-handle spine (Doc 12 §6).
    m.add_function(wrap_pyfunction!(handle::init, m)?)?;
    m.add_function(wrap_pyfunction!(handle::is_initialized, m)?)?;
    m.add_function(wrap_pyfunction!(handle::tars_home, m)?)?;
    m.add_function(wrap_pyfunction!(handle::provider, m)?)?;
    m.add_function(wrap_pyfunction!(handle::pipeline, m)?)?;
    m.add_function(wrap_pyfunction!(handle::role_provider, m)?)?;
    m.add_function(wrap_pyfunction!(handle::context, m)?)?;
    m.add_function(wrap_pyfunction!(handle::resolve_root, m)?)?;
    m.add_function(wrap_pyfunction!(handle::store_dir, m)?)?;
    m.add_class::<context::ContextGuard>()?;
    m.add_class::<Provider>()?;
    m.add_class::<Pipeline>()?;
    m.add_class::<PipelineBuilder>()?;
    m.add_class::<Response>()?;
    m.add_class::<Usage>()?;
    m.add_class::<Telemetry>()?;
    m.add_class::<ValidationSummary>()?;
    m.add_class::<RetryAttemptPy>()?;
    m.add_class::<CompatibilityResult>()?;
    m.add_class::<CompatibilityReasonPy>()?;
    m.add_class::<CapabilityRequirementsPy>()?;
    m.add_class::<validation::PyPass>()?;
    m.add_class::<validation::PyReject>()?;
    m.add_class::<validation::PyFilterText>()?;
    m.add_class::<validation::PyAnnotate>()?;
    m.add_class::<session::Session>()?;
    errors::register(m)?;
    Ok(())
}
